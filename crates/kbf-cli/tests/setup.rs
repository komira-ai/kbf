//! `kbf setup` as `kbf-installer setup`'s twin: the same library, run by the `kbf` binary. A run
//! over a recording host (the same plan, files and accounts as the installer's tests pin: the
//! `kbf.toml` is byte for byte the golden of `kbf-setup/tests/golden/kbf_toml`), a dry run that
//! changes nothing, and the real binary: not root, a usage error, the proxy's flags that it does
//! not take, and its help. The library's own cases (every mode, every refusal) are `kbf-setup`'s.

mod cluster_support;

use std::io;

use cluster_support::kbf;
use kbf_cli::setup::{Args, parse};
use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, HostPath, Owner, Perms, RunUser, Untrusted};
use kbf_setup::database::SchemaOps;
use kbf_setup::layout::Platform;
use kbf_setup::setup::backend::{Backend, Schemas};
use kbf_setup::setup::os::Os;
use kbf_setup::setup::{Options, SetupError, raw_from, run};

/// The `kbf.toml` that `kbf-installer setup --mode=daemon` writes for the flags of [`DAEMON`].
const DAEMON_GOLDEN: &str = include_str!("../../kbf-setup/tests/golden/kbf_toml/daemon.toml");

/// Where setup writes the `kbf.toml` of a Linux host, and where the enrollment token is read.
const KBF_TOML: &str = "/etc/kbf/kbf.toml";
const TOKEN: &str = "/root/kbf.token";

/// The accounts the package makes: `kbf` (the daemon's) and `kbf-server` (the frontend's).
const ACCOUNTS: [(&str, u32); 2] = [("kbf", 990), ("kbf-server", 989)];

/// A daemon host, as the installer's golden case sets it up.
const DAEMON: [&str; 4] = [
    "setup",
    "--mode=daemon",
    "--frontend-url=https://farm.example:8443",
    "--token-file=/root/kbf.token",
];

fn hp(path: &str) -> HostPath {
    HostPath::new(path).expect("a path")
}

/// A Linux host that answers as root, with the package's accounts and nothing else set up.
struct HostOs;

impl Os for HostOs {
    fn euid(&self) -> u32 {
        0
    }

    fn trusted_binary(&self) -> Result<(), Untrusted> {
        Ok(())
    }

    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn user(&self, name: &str) -> io::Result<Option<RunUser>> {
        let account = |(name, id): &(&str, u32)| RunUser {
            name: (*name).to_owned(),
            uid: *id,
            gid: *id,
            groups: vec![*id],
            home: "/nonexistent".to_owned(),
        };
        Ok(ACCOUNTS.iter().find(|(n, _)| *n == name).map(account))
    }

    fn login_shell(&self, _: &str) -> io::Result<String> {
        Ok("/usr/sbin/nologin".to_owned())
    }

    fn subuid(&self) -> io::Result<String> {
        Ok(String::new())
    }

    fn subgid(&self) -> io::Result<String> {
        Ok(String::new())
    }

    fn lingers(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }

    fn unit_enabled(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }

    fn launchd_loaded(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }

    fn dscl_list(&self, _: &str) -> io::Result<String> {
        Ok(String::new())
    }

    fn random(&self, len: usize) -> io::Result<Vec<u8>> {
        Ok(vec![0xab; len])
    }
}

/// No server: the daemon keeps no state in the database, so none is asked for.
struct NoServer;

impl Schemas for NoServer {
    fn of(&self, _: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        Err(SetupError::Database("no server".to_owned()))
    }
}

/// `kbf <words>` parsed as the binary parses it.
fn parsed(words: &[&str]) -> Args {
    let argv: Vec<std::ffi::OsString> = std::iter::once("kbf")
        .chain(words.iter().copied())
        .map(Into::into)
        .collect();
    parse(&argv).expect("a command line")
}

/// The flags `args` say, run over a host with the token file and nothing else: the host after
/// the run, and what the run printed.
fn set_up(args: &Args) -> (FakeExec, String) {
    let mut exec = FakeExec::new(0);
    let token = Entry::File {
        perms: Perms::new(0o600),
        owner: RUNNER,
        bytes: b"tok-abc123\n".to_vec(),
    };
    exec.entries.insert(hp(TOKEN), token);
    let raw = raw_from(&args.setup);
    let options = Options {
        apply: args.apply,
        no_start: args.setup.no_start,
    };
    let backend = Backend { schemas: &NoServer };
    let mut out = Vec::new();
    run(&HostOs, &mut exec, &backend, &raw, options, &mut out).expect("setup");
    (exec, String::from_utf8(out).expect("text"))
}

/// The text of the file at `path`.
fn file_text(exec: &FakeExec, path: &str) -> String {
    match exec.entries.get(&hp(path)) {
        Some(Entry::File { bytes, .. }) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("{path}: {other:?}"),
    }
}

/// The mode and the owner of the file at `path`.
fn file_meta(exec: &FakeExec, path: &str) -> (u32, Owner) {
    match exec.entries.get(&hp(path)) {
        Some(Entry::File { perms, owner, .. }) => (perms.bits(), *owner),
        other => panic!("{path}: {other:?}"),
    }
}

/// Asserts that `kbf setup` with the flags of the installer's daemon case, applied, writes the
/// `kbf.toml` of that case byte for byte, as root's file readable by the `kbf` group (0640), and
/// that the steps it prints include the fetch account and the enrollment token.
#[test]
fn kbf_setup_writes_the_kbf_toml_the_installer_golden_pins() {
    let mut words = DAEMON.to_vec();
    words.push("--apply");
    let (exec, out) = set_up(&parsed(&words));
    assert_eq!(file_text(&exec, KBF_TOML), DAEMON_GOLDEN);
    let (mode, owner) = file_meta(&exec, KBF_TOML);
    assert_eq!(mode, 0o640);
    assert_eq!((owner.uid, owner.gid), (0, 990));
    let steps: Vec<&str> = out.lines().filter(|l| l.starts_with("+ ")).collect();
    assert!(steps.iter().any(|l| l.contains("useradd")), "{out}");
    assert!(steps.iter().any(|l| l.contains("enroll.token")), "{out}");
}

/// Asserts that `kbf setup` without `--apply` is a dry run, as the installer's is: steps are
/// printed as `DRY-RUN:` and none as an applied step (`+ `), the dry-run note is printed, and the
/// host is not touched.
#[test]
fn kbf_setup_without_apply_changes_nothing() {
    let (exec, out) = set_up(&parsed(&DAEMON));
    assert!(exec.log.is_empty());
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    assert!(!out.lines().any(|l| l.starts_with("+ ")), "{out}");
    assert!(out.lines().any(|l| l.starts_with("DRY-RUN: ")), "{out}");
    assert!(out.contains(kbf_host::plan::DRY_RUN_NOTE), "{out}");
}

/// Asserts that `kbf setup` run by a user who is not root is refused as the installer's is: exit
/// 2, nothing on standard output, and a line on standard error that begins with the verb and
/// says to use sudo.
#[test]
fn kbf_setup_is_refused_to_a_user_who_is_not_root() {
    let (code, out, err) = kbf(&DAEMON);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(out, "");
    assert!(err.starts_with("kbf setup: "), "{err}");
    assert!(err.contains("run it with sudo"), "{err}");
}

/// Asserts that with `--json` the refusal of a user who is not root is one object on standard
/// output, with the verb and the stable word of the error, and nothing on standard error.
#[test]
fn kbf_setup_with_json_refuses_a_user_who_is_not_root_as_an_object() {
    let mut words = DAEMON.to_vec();
    words.push("--json");
    let (code, out, err) = kbf(&words);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(err, "");
    let object: serde_json::Value = serde_json::from_str(&out).expect("one JSON object");
    assert_eq!(object["verb"], "setup");
    assert_eq!(object["error"], "needs_root");
}

/// Asserts that the proxy's flags are a usage error of `kbf setup` (exit 2, nothing on standard
/// output, the flag named on standard error): `--proxy` and the flags that go with it are
/// `kbf-installer setup`'s, so the `kbf` binary does not link the proxy.
#[test]
fn the_proxys_flags_are_a_usage_error() {
    for flag in [
        "--proxy",
        "--proxy-base-port=18000",
        "--buck2-auth=none",
        "--buck2-token-file=/x",
        "--storage-port=8990",
    ] {
        let (code, out, err) = kbf(&["setup", "--mode=frontend", flag]);
        let name = flag.split('=').next().unwrap_or_default();
        assert_eq!(code, Some(2), "{flag}: {err}");
        assert_eq!(out, "", "{flag}");
        assert!(err.contains(name), "{flag}: {err}");
    }
}

/// Asserts that a command line without a mode is a usage error that names `--mode`.
#[test]
fn kbf_setup_needs_a_mode() {
    let (code, out, err) = kbf(&["setup"]);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(out, "");
    assert!(err.contains("--mode"), "{err}");
}

/// Asserts that `kbf setup --help` lists the flags of the verb (its own, the library's and
/// the `kbf.toml` settings) and not the proxy's, and says how a proxy is configured (by hand, with
/// `[[proxy.frontend]]` tables); and that `kbf --help` names the verb.
#[test]
fn the_helps_list_the_flags_and_name_the_verb() {
    let (code, out, err) = kbf(&["setup", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("kbf setup"), "{out}");
    for flag in [
        "--mode",
        "--apply",
        "--json",
        "--frontend-url",
        "--token-file",
        "--database",
        "--create-local-database",
        "--no-start",
        "--min-generation",
        "--listen",
        "--storage-url",
        "--installer-config",
    ] {
        assert!(out.contains(flag), "{flag} missing: {out}");
    }
    let proxy_flags: Vec<&str> = out
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("--proxy"))
        .collect();
    assert_eq!(proxy_flags, Vec::<&str>::new());
    assert!(out.contains("[[proxy.frontend]] tables"), "{out}");
    let (code, top, err) = kbf(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(top.contains("kbf setup"), "{top}");
}
