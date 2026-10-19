//! The flags and failure paths of `kbf-installer deploy` that the in-process runs in
//! `tests.rs` do not reach: each flag's value, the host config's read rules, and the errors
//! before a deploy starts. Every run points at a closed loopback port, so a command line that
//! gets past the checks ends in an API failure (exit 1), never in a usage error (exit 2).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use super::cli::{self, FrontendConfig, Host, current_uid, platform_of};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef\n";

struct Rig {
    dir: PathBuf,
    token: PathBuf,
    /// The CA the API's certificate would be verified against.
    ca: PathBuf,
    commit: String,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let (_, dir) = crate::testkit::host_root(&format!("deploy-flags-{name}"));
        let token = dir.join("token");
        fs::write(&token, TOKEN).unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let ca = dir.join("ca.pem");
        fs::write(&ca, crate::deploy::smoke_tests::pki("farm.example").0).unwrap();
        Rig {
            dir,
            token,
            ca,
            commit: "a".repeat(40),
        }
    }

    /// This machine as the tests have it: files belong to the user running them, and the
    /// frontend has no config.
    fn host(&self, root: bool) -> Host {
        Host {
            root,
            config_owner: current_uid(),
            frontend: absent_frontend(self),
        }
    }

    /// The flags every run has: the commit, a closed loopback API, the token and a lock of
    /// this rig's own.
    fn base(&self) -> Vec<String> {
        let lock = self.dir.join("deploy.lock");
        [
            "--commit",
            &self.commit,
            "--api",
            "127.0.0.1:1",
            "--api-ca",
            self.ca.to_str().unwrap(),
            "--api-server-name",
            "farm.example",
            "--token-file",
            self.token.to_str().unwrap(),
            "--lock",
            lock.to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn go(&self, extra: &[&str], stdin: &[u8], root: bool) -> (i32, String, String) {
        self.go_with(self.base(), extra, stdin, &self.host(root))
    }

    fn go_with(
        &self,
        mut args: Vec<String>,
        extra: &[&str],
        mut stdin: &[u8],
        host: &Host,
    ) -> (i32, String, String) {
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::run(&args, &mut stdin, &mut out, &mut err, false, host);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
}

/// A frontend whose config does not exist: nothing to derive a smoke from.
fn absent_frontend(rig: &Rig) -> FrontendConfig {
    FrontendConfig {
        toml: rig.dir.join("no-such-kbf.toml"),
        units: Vec::new(),
    }
}

/// Proves each timeout flag reaches its own parse (a zero is a usage error naming the flag)
/// and a positive value and a list of offline nodes are accepted, and that `--order` is gone:
/// the frontend is always updated first, so a caller that still sends it is told. Catches a
/// flag whose value is dropped, parsed as another flag's, or not parsed, and an order flag
/// that is quietly ignored.
///
/// Mutants planted: `--handover-timeout-s` or `--smoke-timeout-s` ignoring its value (the
/// zero passes); `--order` accepted again.
#[test]
fn every_flag_takes_its_value() {
    let rig = Rig::new("values");
    for flag in [
        "--node-timeout-s",
        "--handover-timeout-s",
        "--smoke-timeout-s",
    ] {
        let (code, out, err) = rig.go(&[flag, "0"], b"", false);
        assert_eq!((code, out.as_str()), (2, ""), "{flag}");
        assert!(err.contains(flag), "{flag}: {err}");
        let (code, _, err) = rig.go(&[flag, "7"], b"", false);
        assert_ne!(code, 2, "{flag}: {err}");
    }
    for order in ["nodes-first", "frontend-first"] {
        let (code, out, err) = rig.go(&["--order", order], b"", false);
        assert_eq!((code, out.as_str()), (2, ""), "{order}");
        assert!(err.contains("unknown flag \"--order\""), "{order}: {err}");
    }
    let (code, _, err) = rig.go(&["--allow-offline", "n1,n2"], b"", false);
    assert_ne!(code, 2, "{err}");
}

/// Proves each run has a request id of its own, in the form the update API accepts (1 to 128
/// letters, digits and `- _ . :`), and that the defaults are the documented ones: a node's
/// budget, the update's (which covers the new frontend's bake), the smoke's and the poll.
/// Catches an id that is the same for every run (the API would answer a new deploy with the
/// update of an old one) and a default that no longer covers the bake.
///
/// Mutants planted: a fixed request id; the handover default 900 s; the node default 600 s.
#[test]
fn each_run_has_its_own_request_id_and_the_documented_timeouts() {
    let rig = Rig::new("request-id");
    let owner = current_uid();
    let a = cli::parse(&rig.base(), false, owner).unwrap().options;
    let b = cli::parse(&rig.base(), false, owner).unwrap().options;
    assert_ne!(a.request_id, b.request_id);
    for id in [&a.request_id, &b.request_id] {
        let hex = id.strip_prefix("deploy-").unwrap();
        assert!(
            hex.len() == 32 && hex.bytes().all(|c| c.is_ascii_hexdigit()),
            "{id}"
        );
    }
    assert_eq!(a.handover_timeout, Duration::from_secs(3600));
    assert_eq!(a.node_timeout, Duration::from_secs(1800));
    assert_eq!(a.smoke_timeout, Duration::from_secs(60));
    assert_eq!(a.poll, Duration::from_secs(5));
}

/// Proves the host config is read only when a flag is missing, and is refused when it is not
/// a regular file or cannot be looked at. Catches a deploy that reads a config it does not
/// need (a broken file would stop a fully flagged run) and one that takes a directory or an
/// unreachable path for an absent config.
///
/// Mutants planted: `needed` always true (the fully flagged run fails on the config); the
/// `is_file` check dropped; an error other than not-found treated as an absent config.
#[test]
fn the_config_is_read_only_when_a_flag_is_missing_and_must_be_a_file() {
    let rig = Rig::new("config");
    let unreachable = "/dev/null/deploy.json";
    let base = |skip: &str| -> Vec<String> {
        let mut list = vec!["--commit".to_owned(), rig.commit.clone()];
        let lock = rig.dir.join("deploy.lock");
        let all = [
            ("--api", "127.0.0.1:1"),
            ("--api-ca", rig.ca.to_str().unwrap()),
            ("--api-server-name", "farm.example"),
            ("--token-file", rig.token.to_str().unwrap()),
            ("--cas", "http://127.0.0.1:1"),
            ("--cas-ca", "/no/such/ca.pem"),
            ("--cas-server-name", "kbf.example"),
        ];
        for (flag, value) in all {
            if flag != skip {
                list.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        list.extend([
            "--lock".to_owned(),
            lock.to_str().unwrap().to_owned(),
            "--config".to_owned(),
            unreachable.to_owned(),
        ]);
        list
    };
    let host = rig.host(false);

    let (code, _, err) = rig.go_with(base(""), &[], b"", &host);
    assert_ne!(code, 2, "every flag given, so no config is read: {err}");
    for skip in [
        "--api",
        "--api-ca",
        "--api-server-name",
        "--token-file",
        "--cas",
        "--cas-ca",
        "--cas-server-name",
    ] {
        let (code, out, err) = rig.go_with(base(skip), &[], b"", &host);
        assert_eq!((code, out.as_str()), (2, ""), "{skip}");
        assert!(err.contains("config"), "{skip}: {err}");
    }

    let dir = rig.dir.to_str().unwrap().to_owned();
    let (code, _, err) = rig.go(&["--config", &dir], b"", false);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("must be a regular file"), "{err}");
}

/// Proves each failure before the deploy starts is a usage error with nothing on standard
/// output: a token file that is missing, a lock that cannot be taken, an API address that is
/// not loopback, and a `--bundle` that is not `-`. A `--bundle -` is read from standard input.
/// Catches a deploy that goes on without a token, without its lock, or toward a remote address.
///
/// Mutants planted: the token read error ignored; the loopback check dropped (the token
/// would go to the address); `--bundle DIR` accepted.
#[test]
fn what_stops_a_deploy_before_it_starts_is_a_usage_error() {
    let rig = Rig::new("early");
    let with = |flag: &str, value: &str| -> Vec<String> {
        let mut args = rig.base();
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1] = value.to_owned();
        args
    };
    let host = rig.host(false);
    let not_a_ca = rig.dir.join("not-a-ca.pem");
    fs::write(&not_a_ca, "not a certificate").unwrap();
    for (args, want) in [
        (with("--token-file", "/no/such/token"), "token file"),
        (with("--lock", "/dev/null/deploy.lock"), "the lock"),
        (with("--api", "0.0.0.0:1"), "loopback"),
        (with("--api-ca", "/no/such/ca.pem"), "--api-ca /no/such"),
        (
            with("--api-ca", not_a_ca.to_str().unwrap()),
            "holds no PEM certificate",
        ),
        (with("--api-server-name", "not a name!"), "not a DNS name"),
    ] {
        let (code, out, err) = rig.go_with(args, &[], b"", &host);
        assert_eq!((code, out.as_str()), (2, ""), "{want}");
        assert!(err.contains(want), "{want}: {err}");
    }
    let (code, out, err) = rig.go(&["--cas", "nonsense"], b"", false);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--cas"), "{err}");
    let (code, out, err) = rig.go(&["--bundle", "some-dir"], b"", false);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--bundle takes -"), "{err}");

    // A bundle on standard input is taken up: the deploy starts, and ends (an empty tar is
    // no bundle) with its one line of summary.
    let (_, out, err) = rig.go(&["--bundle", "-"], b"", false);
    assert!(!err.contains("--bundle takes"), "{err}");
    assert_eq!(out.lines().count(), 1, "{out}");
}

/// Proves `--apply` is refused to anyone but root before the token is read, and that for
/// root the smoke is chosen from the frontend's config: with none the deploy goes on without
/// one, and a CA file that cannot be read is a usage error. Catches an apply by a user the
/// installer's files do not belong to, and a smoke built from a broken flag.
///
/// Mutants planted: the root check dropped; the smoke error ignored (a deploy that
/// verifies nothing).
#[test]
fn apply_wants_root_and_a_smoke_that_can_be_built() {
    let rig = Rig::new("apply");
    let (code, out, err) = rig.go(&["--apply"], b"", false);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--apply must run as root"), "{err}");

    let (code, _, err) = rig.go(&["--apply"], b"", true);
    assert_eq!(code, 1, "no smoke to build, the API is what fails: {err}");

    let (code, out, err) = rig.go(
        &[
            "--apply",
            "--cas",
            "http://127.0.0.1:1",
            "--cas-ca",
            "/no/such/ca.pem",
        ],
        b"",
        true,
    );
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--cas-ca"), "{err}");
}

/// Proves the frontend config of a host is where the units put it, and the platform is
/// spelled as the store spells it for each OS and CPU. Catches a deploy that reads another
/// file than the one the frontend runs from, and a Mac or an Arm host asking for the wrong
/// release.
///
/// Mutants planted: the toml path changed; `aarch64` mapped to x86_64; macOS mapped to linux.
#[test]
fn the_host_paths_and_platform_names() {
    let host = FrontendConfig::host();
    assert_eq!(host.toml, PathBuf::from("/etc/kbf/kbf.toml"));
    let units: Vec<_> = host.units.iter().map(|u| u.to_str().unwrap()).collect();
    assert_eq!(
        units,
        [
            "/etc/systemd/system/kbf-frontend@.service",
            "/etc/systemd/system/kbf-all-in-one.service",
            "/etc/systemd/system/kbf-server.service",
        ]
    );
    assert_eq!(platform_of("macos", "aarch64"), "darwin-arm64");
    assert_eq!(platform_of("macos", "x86_64"), "darwin-arm64");
    assert_eq!(platform_of("linux", "aarch64"), "linux-arm64");
    assert_eq!(platform_of("linux", "x86_64"), "linux-x86_64");
}

/// Proves the host config must belong to the owner the host names (root's, in production):
/// a config of anyone else's is a usage error naming it, before its contents are read.
/// Catches a deploy aimed by a config a user could have written.
///
/// Mutants planted: the owner compared with the wrong id; the check dropped.
#[test]
fn a_config_of_another_owner_is_refused() {
    let rig = Rig::new("owner");
    let config = rig.dir.join("deploy.json");
    fs::write(&config, "{}").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let args = vec![
        "--commit".to_owned(),
        rig.commit.clone(),
        "--config".to_owned(),
        config.to_str().unwrap().to_owned(),
    ];
    let mut host = rig.host(false);
    host.config_owner = current_uid() + 1;
    let (code, out, err) = rig.go_with(args.clone(), &[], b"", &host);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("must be owned by root"), "{err}");
    // Its owner is the one asked for: the config is read, and says nothing of the API.
    host.config_owner = current_uid();
    let (code, _, err) = rig.go_with(args, &[], b"", &host);
    assert_eq!(code, 2);
    assert!(err.contains("--api is required"), "{err}");
}

/// Proves a config the running user owns but cannot read is a usage error naming the config,
/// not an empty config (which would leave the API and token to the flags). Skipped as root,
/// who can read anything.
///
/// Mutants planted: the read error mapped to an empty config.
#[test]
fn a_config_that_cannot_be_read_is_a_usage_error() {
    if current_uid() == 0 {
        return;
    }
    let rig = Rig::new("unreadable");
    let config = rig.dir.join("deploy.json");
    fs::write(&config, "{}").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    let (code, out, err) = rig.go(&["--config", config.to_str().unwrap()], b"", false);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("config:"), "{err}");
}
