//! The flags of the OCI front: what they make, what refuses to start, and the exit status of
//! each refusal.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_testkit::dir::TestDir;

use super::{Cli, Error, build, exit_status, oci_cut_note, run_serving};
use crate::server::oci::test_pki::{Leaf, ca, dns, ip, leaf};

/// A directory with a key file and a certificate and key that name 127.0.0.1.
struct Rig {
    _dir: TestDir,
    key: String,
    leaf: Leaf,
    dir: std::path::PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let key = dir.path().join("key");
        std::fs::write(&key, "AKID\nsecret\n").expect("write");
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).expect("mode");
        let leaf = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
        let path = dir.path().to_owned();
        Self {
            _dir: dir,
            key: key.display().to_string(),
            leaf,
            dir: path,
        }
    }

    /// The flags of a start with the OCI front on, then `more`.
    fn args(&self, more: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = [
            "tierstore",
            "--listen",
            "127.0.0.1:0",
            "--proxy-id",
            "p1",
            "--hot",
            "memory",
            "--cold",
            "memory",
            "--hot-bytes",
            "1000",
            "--bucket",
            "tier-bucket",
            "--access-key-file",
            self.key.as_str(),
            "--oci-listen",
            "127.0.0.1:0",
            "--oci-tls-cert",
            self.leaf.cert.to_str().expect("a path"),
            "--oci-tls-key",
            self.leaf.key.to_str().expect("a path"),
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(more.iter().map(|flag| (*flag).to_owned()));
        args
    }

    fn cli(&self, more: &[&str]) -> Cli {
        Cli::try_parse_from(self.args(more)).expect("flags")
    }
}

/// The exit status and the message of the start `cli` makes.
fn refused(cli: &Cli) -> (u8, String) {
    let error: Error = build(cli).err().expect("a refused start");
    (exit_status(&error), error.to_string())
}

/// Asserts that the front is off without `--oci-listen`: nothing is made, and `/metricz` has no
/// counters to show.
#[test]
fn there_is_no_front_without_its_flags() {
    let rig = Rig::new("oci-flags-off");
    let args = rig.args(&[]);
    let off = &args[..args.len() - 6];
    let cli = Cli::try_parse_from(off).expect("flags");
    let built = build(&cli).expect("built");
    assert!(built.oci.is_none());
    assert!(built.server.oci_metrics.is_none());
}

/// Asserts the settings the flags make: the defaults (prefix `oci/`, 256 connections, an hour,
/// the S3 front's header and idle timeouts) and the values given, and that the counters in the
/// server's settings are the front's.
#[test]
fn the_flags_make_the_front() {
    let rig = Rig::new("oci-flags-on");
    let built = build(&rig.cli(&[])).expect("built");
    let setup = built.oci.expect("a front");
    assert_eq!(setup.prefix.as_str(), "oci/");
    assert_eq!(setup.listen.to_string(), "127.0.0.1:0");
    assert_eq!(setup.limits.max_connections, 256);
    assert_eq!(setup.limits.lifetime, Duration::from_secs(3600));
    assert_eq!(setup.limits.handshake_timeout, Duration::from_secs(10));
    assert_eq!(setup.limits.header_timeout, built.server.header_timeout);
    assert_eq!(setup.limits.write_idle, built.server.body_idle_timeout);
    assert_eq!(setup.limits.drain_deadline, built.server.drain_deadline);
    let shown = built.server.oci_metrics.expect("counters");
    assert!(Arc::ptr_eq(&shown, &setup.metrics));

    let given = ["--oci-prefix", "images/v1/", "--oci-max-connections", "7"];
    let given = [&given[..], &["--oci-connection-lifetime", "90s"]].concat();
    let built = build(&rig.cli(&given)).expect("built");
    let setup = built.oci.expect("a front");
    assert_eq!(setup.prefix.as_str(), "images/v1/");
    assert_eq!(setup.limits.max_connections, 7);
    assert_eq!(setup.limits.lifetime, Duration::from_secs(90));
}

/// Asserts the flags that need `--oci-listen` are refused without it, and `--oci-listen` is
/// refused without a certificate and a key (there is no plain-HTTP mode), and values that
/// cannot be right are refused by the parser.
#[test]
fn the_oci_flags_go_together() {
    let rig = Rig::new("oci-flags-parse");
    let args = rig.args(&[]);
    let base = &args[..args.len() - 6];
    let with = |more: &[&str]| {
        let mut all = base.to_vec();
        all.extend(more.iter().map(|flag| (*flag).to_owned()));
        Cli::try_parse_from(all)
    };
    let cert = rig.leaf.cert.to_str().expect("a path");
    let key = rig.leaf.key.to_str().expect("a path");
    assert!(with(&["--oci-listen", "127.0.0.1:0"]).is_err());
    assert!(with(&["--oci-listen", "127.0.0.1:0", "--oci-tls-cert", cert]).is_err());
    assert!(with(&["--oci-listen", "127.0.0.1:0", "--oci-tls-key", key]).is_err());
    assert!(with(&["--oci-tls-cert", cert]).is_err());
    assert!(with(&["--oci-tls-key", key]).is_err());
    assert!(with(&["--oci-prefix", "oci/"]).is_err());
    assert!(with(&["--oci-max-connections", "1"]).is_err());
    assert!(with(&["--oci-connection-lifetime", "1h"]).is_err());
    assert!(Cli::try_parse_from(rig.args(&["--oci-max-connections", "0"])).is_err());
    assert!(Cli::try_parse_from(rig.args(&["--oci-connection-lifetime", "soon"])).is_err());
    assert!(with(&[]).is_ok());
}

/// Asserts that a prefix that cannot work, a lifetime of nothing, and a certificate or key that
/// cannot serve are refused at start with status 2, and that a file that cannot be read is
/// status 1, which a restart may mend.
#[test]
fn a_front_that_cannot_serve_is_refused_at_start() {
    let rig = Rig::new("oci-flags-refused");
    for prefix in ["_journal/", "_tiering/x/", "oci", "", "a//b/", "../"] {
        let (status, why) = refused(&rig.cli(&["--oci-prefix", prefix]));
        assert_eq!(status, 2, "{prefix:?}: {why}");
        assert!(why.contains("--oci-prefix"), "{why}");
    }
    let open = rig.dir.join("open.key");
    std::fs::copy(&rig.leaf.key, &open).expect("copy");
    std::fs::set_permissions(&open, Permissions::from_mode(0o644)).expect("mode");
    let mut cli = rig.cli(&[]);
    cli.oci_tls_key = Some(open);
    let (status, why) = refused(&cli);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("any user can read the key"), "{why}");
    let (status, why) = refused(&rig.cli(&["--oci-connection-lifetime", "0s"]));
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("--oci-connection-lifetime"), "{why}");

    let other = leaf(
        &rig.dir,
        "other",
        &ca(),
        vec![ip("192.0.2.99"), dns("registry.test")],
    );
    let mut cli = rig.cli(&[]);
    cli.oci_tls_cert = Some(other.cert);
    cli.oci_tls_key = Some(other.key);
    let (status, why) = refused(&cli);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("no IP subjectAltName for 127.0.0.1"), "{why}");

    let mut cli = rig.cli(&[]);
    cli.oci_tls_cert = Some(rig.dir.join("missing.pem"));
    assert_eq!(refused(&cli).0, 1);
    let mut cli = rig.cli(&[]);
    cli.oci_tls_key = Some(rig.dir.join("missing.key"));
    assert_eq!(refused(&cli).0, 1);

    // The parser does not let these through; a settings struct built by hand might.
    for drop in [0, 1] {
        let mut cli = rig.cli(&[]);
        match drop {
            0 => cli.oci_tls_cert = None,
            _ => cli.oci_tls_key = None,
        }
        let (status, why) = refused(&cli);
        assert_eq!(status, 2, "{why}");
        assert!(why.contains("https only"), "{why}");
    }
}

/// Asserts that an OCI address that is in use fails the start with status 1 (a restart may mend
/// it), and the message names the address.
#[tokio::test]
async fn an_oci_address_in_use_is_status_1() {
    let rig = Rig::new("oci-flags-in-use");
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("a socket");
    let addr = held.local_addr().expect("an address").to_string();
    let mut args = rig.args(&[]);
    let at = args
        .iter()
        .position(|arg| arg == "--oci-listen")
        .expect("flag");
    args[at + 1] = addr.clone();
    let cli = Cli::try_parse_from(args).expect("flags");
    let stop = std::future::pending::<()>();
    let error = run_serving(&cli, stop, |_| ()).await.expect_err("refused");
    assert_eq!(exit_status(&error), 1);
    assert!(
        error.to_string().starts_with(&format!("binding {addr}: ")),
        "{error}"
    );
}

/// Asserts that a bucket called `metricz` is refused like the other probe paths.
#[test]
fn the_metrics_path_is_not_a_bucket_name() {
    let rig = Rig::new("oci-flags-bucket");
    let (status, why) = refused(&rig.cli(&["--bucket", "metricz"]));
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("probe path"), "{why}");
}

/// Asserts what is said of OCI connections cut at the drain deadline: nothing for none.
#[test]
fn the_cut_connections_are_reported() {
    assert_eq!(oci_cut_note(0), "");
    assert_eq!(
        oci_cut_note(3),
        "tierstore: 3 OCI connections were cut at the drain deadline\n"
    );
}

/// Asserts that the help names the flags of the OCI front, and the defaults it states.
#[test]
fn the_help_names_the_oci_flags() {
    use clap::CommandFactory as _;
    let help = Cli::command().render_long_help().to_string();
    for flag in [
        "--oci-listen",
        "--oci-tls-cert",
        "--oci-tls-key",
        "--oci-prefix",
        "--oci-max-connections",
        "--oci-connection-lifetime",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
    assert!(help.contains("`oci/` if not given"));
}
