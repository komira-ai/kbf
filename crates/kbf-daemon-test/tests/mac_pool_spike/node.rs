//! What the spike needs around the daemon: a scratch directory every pool user may search,
//! the node's certificates, the `kbf` binary, and the daemon as a process that is killed
//! when its guard drops.
//!
//! The certificates and the daemon's words follow kbf-cli's `tests/macos_daemon.rs`, which
//! starts the same daemon without a pool as the runner's user; this one is root's.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;

use kbf_deploy::linux::verify_cert::{CLIENT_AUTH, ROLE_URI_PREFIX, Row, verify};
use kbf_testkit::dir::TestDir;
use rcgen::{
    CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};

/// A directory under `/private/tmp`, root's and mode 0755 (the runner's home is not
/// searchable by a pool user, so nothing a pool user must reach lives there), removed when
/// it drops. It holds a copy of this test binary as `kbf-act`, a copy of `kbf`, so an action can run the act
/// steps, the node's certificates (`tls/`), the daemon's data directory (`data/`) and its
/// log.
pub struct Scratch(TestDir);

impl Scratch {
    pub fn new() -> Self {
        let dir = TestDir::under("/private/tmp", "kbf-pool-spike");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let act = dir.join(kbf_testkit::act::NAME);
        std::fs::copy(std::env::current_exe().expect("this binary"), &act).expect("copy");
        std::fs::set_permissions(&act, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        // `kbf` too: the daemon's native driver runs its own executable as the `__as-user`
        // program for each pool user, and a pool user cannot search the runner's home.
        let kbf_copy = dir.join("kbf");
        std::fs::copy(kbf(), &kbf_copy).expect("copy kbf");
        std::fs::set_permissions(&kbf_copy, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        tls(&dir.join("tls"));
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// The copy of this binary that runs act steps.
    pub fn act(&self) -> String {
        self.path()
            .join(kbf_testkit::act::NAME)
            .to_string_lossy()
            .into_owned()
    }

    /// The copy of `kbf` a pool user can run.
    pub fn kbf(&self) -> PathBuf {
        self.path().join("kbf")
    }

    pub fn tls(&self) -> PathBuf {
        self.path().join("tls")
    }

    pub fn data(&self) -> PathBuf {
        self.path().join("data")
    }

    /// `<data>/leases`, where a lease's directory is made.
    pub fn leases(&self) -> PathBuf {
        self.data().join("leases")
    }

    pub fn log(&self) -> PathBuf {
        self.path().join("daemon.log")
    }
}

/// Writes a CA, a daemon certificate and the fake front's certificate (for `localhost`) into
/// `dir` (made, mode 0755; the keys 0600).
///
/// The node certificate is a daemon's as the farm's issuer makes one, because `kbf`'s startup
/// self-checks refuse any other: the node id `node-1` as its DNS name, the role URI
/// `urn:kbf:role:daemon`, and the `clientAuth` extended key usage. It is checked here by
/// `kbf-deploy linux verify-cert`'s function, so a certificate the daemon would refuse fails
/// at issue with the reason.
fn tls(dir: &Path) {
    std::fs::create_dir_all(dir).expect("tls directory");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [("node", "node-1"), ("server", "localhost")] {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(vec![dns.to_owned()]).expect("params");
        if name == "node" {
            params.subject_alt_names.push(SanType::URI(
                format!("{ROLE_URI_PREFIX}{}", Row::Daemon.role())
                    .try_into()
                    .expect("IA5"),
            ));
            params.is_ca = IsCa::ExplicitNoCa;
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = Row::Daemon
                .ekus()
                .iter()
                .map(|e| {
                    assert_eq!(*e, CLIENT_AUTH, "the daemon row asks for clientAuth only");
                    ExtendedKeyUsagePurpose::ClientAuth
                })
                .collect();
            params.use_authority_key_identifier_extension = true;
        }
        let cert = params.signed_by(&key, &ca).expect("sign");
        if name == "node" {
            let failures = verify(
                ca.pem().as_bytes(),
                cert.pem().as_bytes(),
                Row::Daemon,
                Some("node-1"),
                SystemTime::now(),
            )
            .expect("verify-cert checks the node certificate");
            assert!(failures.is_empty(), "node.pem is no daemon's: {failures:?}");
        }
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        // Private to this user: the startup self-checks refuse a key the group or others
        // can read.
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&key_path, key.serialize_pem()).expect("write");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
}

/// The `kbf` binary built beside this test binary (`cargo build -p kbf-cli --bin kbf`):
/// in the nearest directory above it that holds one, which is `target/debug` for a test
/// binary in `deps/`.
pub fn kbf() -> PathBuf {
    let exe = std::env::current_exe().expect("this test binary");
    exe.ancestors()
        .skip(1)
        .map(|dir| dir.join("kbf"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "kbf is not built above {}: `cargo build -p kbf-cli --bin kbf --locked` first",
                exe.display()
            )
        })
}

/// A daemon process: SIGKILLed and reaped when dropped, so a failed assertion leaves no
/// daemon running. Only this pid is signalled.
pub struct Daemon(Child);

impl Daemon {
    /// `kbf daemon --driver native` with a pool of `users` from `first_uid`, as the Mac's
    /// launchd unit starts it (the words of `daemon_args` in the install kit, as kbf-cli's
    /// `tests/macos_daemon.rs` writes them), against the fake front on `port`, with the
    /// scratch directory's certificates and data directory, and an empty settings file
    /// (it reads no host `kbf.toml`). Its stderr is the scratch directory's log.
    pub fn start(scratch: &Scratch, port: u16, users: usize, first_uid: u32) -> Self {
        let dir = scratch.tls();
        let at = |name: &str| dir.join(name).display().to_string();
        let url = format!("https://localhost:{port}");
        let child = Command::new(scratch.kbf())
            .args([
                "daemon",
                "--driver",
                "native",
                "--server",
                &url,
                "--cas",
                &url,
                "--ca-cert",
                &at("ca.pem"),
                "--cert",
                &at("node.pem"),
                "--key",
                &at("node.key"),
                "--node-id",
                "node-1",
                "--data-dir",
                &scratch.data().display().to_string(),
                "--data-max-bytes",
                "4294967296",
                "--pool-users",
                &users.to_string(),
                "--pool-first-uid",
                &first_uid.to_string(),
            ])
            .arg(kbf_testkit::config::flag(scratch.path()))
            .current_dir(scratch.path())
            .env("HOME", scratch.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(scratch.log()).expect("log file"))
            .spawn()
            .expect("spawn kbf daemon");
        Self(child)
    }

    pub fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether the daemon has ended, and how.
    pub fn ended(&mut self) -> Option<std::process::ExitStatus> {
        self.0.try_wait().ok().flatten()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // The process may have ended between the two calls; a destructor that panics
        // during a panic aborts the test binary.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
