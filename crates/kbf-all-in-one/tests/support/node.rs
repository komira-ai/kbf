//! What the tests that run a node binary share: its certificates, the stand-in
//! `crun` and cgroup tree its container driver runs over, and the checks of what is
//! on its disk. Used by this crate's tests/binary.rs and by kbf-cli's
//! tests/storage_and_daemon.rs (through `#[path]`: a test can run only its own
//! package's binaries, so each package that runs a node has a test of its own).

#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType};

// The stand-in `crun`, the planted cgroup tree and id files, the disk scan and the large
// input live in kbf-harness, which the rehearsal farm uses too.
#[allow(unused_imports)] // Each test binary uses its own part.
pub(crate) use kbf_harness::node_support::{
    MANIFEST, cgroup_tree, crun, files_holding, id_files, image_path, input_bytes, oci_runtime,
};

/// How long the test waits for what should happen promptly.
pub(crate) const PROMPT: Duration = Duration::from_secs(60);

/// Where tests that run a container daemon put their directories: Cargo's per-target
/// temporary directory, unless an ancestor of it is closed to other users (a home
/// directory often is), which the container driver refuses (its containers' ids are not
/// the daemon's, and must be able to search down to the data directory); then the
/// system's temporary directory.
pub(crate) fn scratch_root() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let target = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let searchable = std::fs::canonicalize(&target).is_ok_and(|real| {
        real.ancestors()
            .all(|a| std::fs::metadata(a).is_ok_and(|m| m.permissions().mode() & 0o001 != 0))
    });
    if searchable {
        target
    } else {
        std::env::temp_dir()
    }
}

/// A CA, the node's certificate (`node-1`, its one DNS name, the daemon role its worker
/// session needs on the worker listener, and clientAuth, the daemon row of the farm's
/// table), the server's (`localhost`: role `storage` and both key usages, the storage row,
/// which is also what the in-process scheduler presents as a TLS server), and a storage
/// client's (`client.pem`, naming the role `frontend` as a storage listener under mutual
/// TLS requires), written into `dir`; every key private to this user, as `kbf`'s startup
/// self-checks want them.
pub(crate) fn pki(dir: &Path) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [("node", "node-1"), ("server", "localhost")] {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(vec![dns.to_owned()]).expect("params");
        let (role, ekus) = if name == "node" {
            ("daemon", vec![ExtendedKeyUsagePurpose::ClientAuth])
        } else {
            (
                "storage",
                vec![
                    ExtendedKeyUsagePurpose::ServerAuth,
                    ExtendedKeyUsagePurpose::ClientAuth,
                ],
            )
        };
        let role = format!("{}{role}", kbf_storage::service::ROLE_URI_PREFIX);
        params
            .subject_alt_names
            .push(SanType::URI(role.try_into().expect("IA5")));
        params.extended_key_usages = ekus;
        let cert = params.signed_by(&key, &ca).expect("sign");
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&key_path, key.serialize_pem()).expect("write");
        std::fs::set_permissions(
            &key_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("mode");
    }
    let mut client = CertificateParams::new(Vec::<String>::new()).expect("params");
    let role = format!("{}frontend", kbf_storage::service::ROLE_URI_PREFIX);
    client.subject_alt_names = vec![SanType::URI(role.try_into().expect("IA5"))];
    let key = KeyPair::generate().expect("key");
    let cert = client.signed_by(&key, &ca).expect("sign");
    std::fs::write(dir.join("client.pem"), cert.pem()).expect("write");
    std::fs::write(dir.join("client.key"), key.serialize_pem()).expect("write");
}

/// An address nothing listens on now, for a worker listener the binary binds itself,
/// that no other test or process is likely to take before the binary does. The port
/// cannot be left to the binary (the daemon half dials it by the `--server` flag), and a
/// port number probed on `127.0.0.1` is free only until a test binding `127.0.0.1:0`, or
/// a connection's source port, takes it. So the address is a loopback address of this
/// call's own (127 and three more bytes; the whole loopback block is the host's own on
/// Linux) that nothing else binds, with a port probed on it: the one thing that could
/// take it is a process that chose the same address.
pub(crate) fn worker_addr() -> SocketAddr {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    assert!(call < 15, "more addresses than one process can tell apart");
    // 23 bits of process and call number under a leading 1 bit: the second byte is never
    // 0, so never the usual loopback address, and with a call number under 15 the three
    // bytes are never all ones (the block's broadcast address).
    let host = ((std::process::id().wrapping_mul(16) + call) & 0x7F_FFFF) | 0x80_0000;
    let [_, a, b, c] = host.to_be_bytes();
    let port = std::net::TcpListener::bind(SocketAddr::from(([127, a, b, c], 0)))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    SocketAddr::from(([127, a, b, c], port))
}

/// Kills the process when dropped, so a failed test leaves nothing running.
pub(crate) struct Running(pub(crate) Child);

impl Running {
    /// Waits for the process to end on its own, for at most [`PROMPT`].
    pub(crate) fn wait(mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + PROMPT;
        loop {
            if let Some(status) = self.0.try_wait().expect("wait") {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
