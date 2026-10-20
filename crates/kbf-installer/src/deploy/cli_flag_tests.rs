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
            installer_root: self.dir.clone(),
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
            "--result",
            self.dir.join("result.json").to_str().unwrap(),
            "--observe",
            "127.0.0.1:2",
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
/// Mutants planted: `--handover-timeout-s`, `--smoke-timeout-s` or `--settle-timeout-s` ignoring
/// its value (the zero passes); `--set-hash` accepted unparsed; `--order` accepted again.
#[test]
fn every_flag_takes_its_value() {
    let rig = Rig::new("values");
    for flag in [
        "--node-timeout-s",
        "--handover-timeout-s",
        "--smoke-timeout-s",
        "--settle-timeout-s",
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
    // The set hash is 64 lower-case hex digits.
    let (code, out, err) = rig.go(&["--set-hash", "ABC"], b"", false);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--set-hash"), "{err}");
    let (code, _, err) = rig.go(&["--set-hash", &"ab".repeat(32)], b"", false);
    assert_ne!(code, 2, "{err}");
}

/// Proves an applied deploy that is not told where the gates read does not start (exit 2,
/// naming `--observe`), a plan does not need it, `--observe` must be loopback and an address,
/// and `--observe-token-file` is read like the token file. Catches a deploy that defaults the
/// gate reads to `--api` (which does not serve them) or reads them from a remote address.
///
/// Mutants planted: the missing `--observe` allowed with `--apply`.
#[test]
fn an_applied_deploy_must_name_the_frontend_it_observes() {
    let rig = Rig::new("observe");
    let without: Vec<String> = {
        let mut a = rig.base();
        let at = a.iter().position(|x| x == "--observe").unwrap();
        a.drain(at..=at + 1);
        a
    };
    let host = rig.host(true);
    let (code, out, err) = rig.go_with(without.clone(), &["--apply"], b"", &host);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.contains("--observe or --observe-base-port is required"),
        "{err}"
    );
    let mut both = rig.base();
    both.extend(["--observe-base-port".to_owned(), "18000".to_owned()]);
    let (code, _, err) = rig.go_with(both, &["--apply"], b"", &host);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("give one"), "{err}");
    for bad in ["80", "65535", "x"] {
        let mut a = rig.base();
        let at = a.iter().position(|x| x == "--observe").unwrap();
        a.splice(
            at..=at + 1,
            ["--observe-base-port".to_owned(), bad.to_owned()],
        );
        let (code, _, err) = rig.go_with(a, &["--apply"], b"", &host);
        assert_eq!(code, 2, "{bad}: {err}");
        assert!(err.contains("--observe-base-port"), "{bad}: {err}");
    }
    let (code, _, err) = rig.go_with(without, &[], b"", &host);
    assert_ne!(code, 2, "a plan needs no --observe: {err}");
    for bad in ["0.0.0.0:2", "not-an-address"] {
        let mut a = rig.base();
        let at = a.iter().position(|x| x == "--observe").unwrap();
        a[at + 1] = bad.to_owned();
        let (code, _, err) = rig.go_with(a, &["--apply"], b"", &host);
        assert_eq!(code, 2, "{bad}: {err}");
        assert!(err.contains("--observe"), "{bad}: {err}");
    }
    let (code, _, err) = rig.go(&["--observe-token-file", "/no/such/token"], b"", false);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--observe-token-file"), "{err}");
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
            ("--stage", "gamma"),
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
            "--no-offer".to_owned(),
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
        "--stage",
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
    let (code, out, err) = rig.go(&["--apply", "--stage", "gamma", "--no-offer"], b"", false);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--apply must run as root"), "{err}");

    let (code, _, err) = rig.go(&["--apply", "--stage", "gamma", "--no-offer"], b"", true);
    assert_eq!(code, 1, "no smoke to build, the API is what fails: {err}");

    let (code, out, err) = rig.go(
        &[
            "--apply",
            "--stage",
            "gamma",
            "--no-offer",
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

// ---- the offer flags -----------------------------------------------------------------

use super::offer::{OfferPolicy, Stage};

impl Rig {
    /// `parse` of the base flags and `extra`, not through sudo.
    fn parse(&self, extra: &[&str]) -> Result<cli::Args, String> {
        let mut args = self.base();
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        cli::parse(&args, false, current_uid())
    }

    fn policy(&self, extra: &[&str]) -> Result<OfferPolicy, String> {
        self.parse(extra).map(|a| a.options.offer)
    }

    /// A valid gamma offer in a file of its own.
    fn offer_file(&self) -> String {
        let doc = serde_json::json!({
            "version": 1, "created": 1_790_000_000u64, "target": "gamma", "commit": self.commit,
            "set_hash": "f".repeat(64), "platforms": ["linux-x86_64"],
            "beta": {"kind": "attested", "run_id": "1"},
            "meta_log": 4, "previous": "b".repeat(40), "previous_meta_log": 4, "reversible": true,
        });
        let path = self.dir.join("offer.json");
        fs::write(&path, doc.to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

/// (b) Proves how the deploy knows its stage: `--stage gamma|prod`, else the host config's
/// `stage`, and nothing else (not the offer). An apply, an `--offer` or a `--no-offer` with
/// no stage is a usage error, so no offer rule is skipped by leaving the stage out; a plan
/// with none is unchecked. Catches an apply that runs with no stage, a stage the config does
/// not supply, and a stage spelled as anything but the two.
///
/// Mutants planted: the apply-needs-stage check removed; the config key `stage` not read; a
/// third stage word accepted.
#[test]
fn the_stage_comes_from_the_flag_or_the_host_config() {
    let rig = Rig::new("stage");
    assert!(matches!(rig.policy(&[]), Ok(OfferPolicy::Unchecked)));
    for extra in [
        &["--apply"][..],
        &["--no-offer"],
        &["--offer", "/x/offer.json"],
    ] {
        let err = rig.policy(extra).unwrap_err();
        assert!(err.contains("--stage"), "{extra:?}: {err}");
    }
    assert!(
        rig.policy(&["--stage", "beta"])
            .unwrap_err()
            .contains("gamma or prod")
    );
    assert!(matches!(
        rig.policy(&["--stage", "prod", "--no-offer"]),
        Ok(OfferPolicy::Waived(Stage::Prod))
    ));
    let config = rig.dir.join("deploy.json");
    fs::write(&config, r#"{"stage": "prod"}"#).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let from_config = rig.policy(&["--config", config.to_str().unwrap(), "--no-offer"]);
    assert!(
        matches!(from_config, Ok(OfferPolicy::Waived(Stage::Prod))),
        "{from_config:?}"
    );
}

/// (g) Proves a named stage with no `--offer` is `Missing` (the deploy then refuses), that
/// `--no-offer` is the only way past, and that `--offer` with `--no-offer` is a usage error.
///
/// Mutants planted: the default made `Waived`; the exclusion removed.
#[test]
fn a_stage_without_an_offer_is_missing_unless_waived() {
    let rig = Rig::new("missing");
    assert!(matches!(
        rig.policy(&["--stage", "gamma"]),
        Ok(OfferPolicy::Missing(Stage::Gamma))
    ));
    assert!(matches!(
        rig.policy(&["--stage", "gamma", "--no-offer"]),
        Ok(OfferPolicy::Waived(Stage::Gamma))
    ));
    let both = rig.policy(&[
        "--stage",
        "gamma",
        "--no-offer",
        "--offer",
        &rig.offer_file(),
    ]);
    assert!(both.unwrap_err().contains("exclude"));
}

/// (a, e) Proves `--offer` reads the file through the validated parse (a file that is not an
/// offer is a usage error naming the flag) and the age limit defaults to 72 hours, is
/// `--offer-max-age-h` when given, and cannot be zero.
///
/// Mutants planted: the default 72 made 720; the flag's value ignored.
#[test]
fn the_offer_flag_reads_the_file_and_the_age_limit_has_a_default() {
    let rig = Rig::new("offerflag");
    let file = rig.offer_file();
    let max_age = |extra: &[&str]| {
        let mut flags = vec!["--stage", "gamma", "--offer", &file];
        flags.extend_from_slice(extra);
        match rig.policy(&flags) {
            Ok(OfferPolicy::Offer { max_age, .. }) => max_age,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(max_age(&[]), Duration::from_secs(72 * 3600));
    assert_eq!(
        max_age(&["--offer-max-age-h", "5"]),
        Duration::from_secs(5 * 3600)
    );
    assert!(
        rig.policy(&[
            "--stage",
            "gamma",
            "--offer",
            &file,
            "--offer-max-age-h",
            "0"
        ])
        .is_err()
    );
    let junk = rig.dir.join("junk.json");
    fs::write(&junk, "{}").unwrap();
    let err = rig
        .policy(&["--stage", "gamma", "--offer", junk.to_str().unwrap()])
        .unwrap_err();
    assert!(err.contains("--offer"), "{err}");
}

/// (g) Proves end to end that a gamma apply with no offer ends in a refusal on the summary
/// line (exit 3, `offer_required`) before any call to the API (which here is a closed port),
/// and that `--no-offer` goes on to the API (which then fails, exit 1) and says so on the
/// summary. Catches a deploy that reaches the farm without an offer.
///
/// Mutant planted: the missing-offer step let through.
#[test]
fn a_gamma_apply_with_no_offer_is_refused_before_the_farm_is_asked() {
    let rig = Rig::new("nooffer");
    let (code, out, _) = rig.go(&["--apply", "--stage", "gamma"], b"", true);
    assert_eq!(code, 3, "{out}");
    let line: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(line["reason"], "offer_required");
    assert_eq!(line["steps"].as_array().unwrap().len(), 1, "{out}");
    let (code, out, err) = rig.go(&["--apply", "--stage", "gamma", "--no-offer"], b"", true);
    assert_eq!(code, 1, "{out} {err}");
    assert!(err.contains("--no-offer"), "{err}");
    let line: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(line["offer"]["kind"], "waived");
}

/// Proves a gamma apply with a valid but old offer gets as far as reading the offer and
/// building the probe, and ends in the offer's own refusal (exit 3, `offer_stale`) on the
/// summary line; and that settings the host does not trust stop the deploy before it starts
/// (exit 2), since the user the probe runs as comes from them. Catches an offer that is parsed
/// and then not applied, and a probe built from settings nobody vouched for.
///
/// Mutant planted: the probe error ignored (the deploy goes on with no probe).
#[test]
fn a_stale_offer_ends_the_deploy_and_untrusted_settings_stop_it() {
    let rig = Rig::new("stale");
    let (code, out, _) = rig.go(
        &["--apply", "--stage", "gamma", "--offer", &rig.offer_file()],
        b"",
        true,
    );
    assert_eq!(code, 3, "{out}");
    let line: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(line["reason"], "offer_stale");
    assert_eq!(line["offer"]["kind"], "offer");
    if current_uid() == 0 {
        return;
    }
    // The deploy reads the settings of the platform it runs on (`HostProbe::at`), so they are
    // planted where that platform's layout keeps them.
    let layout = crate::layout::Layout::new(
        kbf_host::Root::new(&rig.dir).unwrap(),
        crate::layout::Platform::current(),
    );
    let settings = layout.join(&layout.settings());
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, "RUN_AS_UID=65534\n").unwrap();
    let (code, out, err) = rig.go(
        &["--apply", "--stage", "gamma", "--offer", &rig.offer_file()],
        b"",
        true,
    );
    assert_eq!((code, out.as_str()), (2, ""), "{err}");
    assert!(
        err.contains("installer.conf") || err.contains("settings"),
        "{err}"
    );
}
