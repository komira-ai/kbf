//! The self-checks against a planted host: one fault per test, so each test names the
//! check that would catch it. `daemon_needs` and `Fake::healthy` are a daemon that
//! passes everything; a test breaks one thing and expects exactly one problem.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rcgen::{ExtendedKeyUsagePurpose as Eku, KeyUsagePurpose, SanType};

use super::host::{Kind, Output, Stat};
use super::{
    CertNeed, Class, ContainerNeed, FileNeed, Host, NOT_READY, Needs, REFUSED, Report, Row, check,
};

const ME: u32 = 1000;
const CA: &str = "/etc/kbf/ca.pem";
const CERT: &str = "/etc/kbf/node.pem";
const KEY: &str = "/etc/kbf/node.key";
const CRUN: &str = "/usr/bin/crun";

// ---- a planted host -------------------------------------------------------------------

#[derive(Clone)]
struct Fake {
    files: BTreeMap<PathBuf, (Stat, Vec<u8>)>,
    stat_errors: BTreeMap<PathBuf, io::ErrorKind>,
    vars: BTreeMap<String, String>,
    euid: u32,
    nofile: u64,
    programs: BTreeMap<PathBuf, Result<(bool, String), io::ErrorKind>>,
}

impl Fake {
    fn empty() -> Self {
        Self {
            files: BTreeMap::new(),
            stat_errors: BTreeMap::new(),
            vars: BTreeMap::new(),
            euid: ME,
            nofile: 1 << 20,
            programs: BTreeMap::new(),
        }
    }

    fn put(mut self, path: &str, kind: Kind, uid: u32, mode: u32, bytes: &[u8]) -> Self {
        self.files.insert(
            PathBuf::from(path),
            (Stat { kind, uid, mode }, bytes.to_vec()),
        );
        self
    }

    fn file(self, path: &str, mode: u32, bytes: &[u8]) -> Self {
        self.put(path, Kind::File, ME, mode, bytes)
    }

    fn var(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.to_owned(), value.to_owned());
        self
    }

    /// Changes the owner and mode of a file already there, keeping its bytes.
    fn chmod(mut self, path: &str, uid: u32, mode: u32) -> Self {
        let (stat, _) = self.files.get_mut(Path::new(path)).expect("file there");
        stat.uid = uid;
        stat.mode = mode;
        self
    }

    fn without(mut self, path: &str) -> Self {
        self.files.remove(Path::new(path));
        self
    }

    /// A host a daemon with the container driver passes on: its certificate files, Podman
    /// and crun, the user's runtime directory.
    fn healthy(cert_pem: &str, key_pem: &str, ca_pem: &str) -> Self {
        let mut f = Self::empty()
            .file(CA, 0o644, ca_pem.as_bytes())
            .file(CERT, 0o644, cert_pem.as_bytes())
            .file(KEY, 0o600, key_pem.as_bytes())
            .file("/usr/bin/podman", 0o755, b"")
            .file(CRUN, 0o755, b"")
            .put("/run/user/1000", Kind::Dir, ME, 0o700, b"")
            .var("PATH", "/usr/local/bin:/usr/bin")
            .var("XDG_RUNTIME_DIR", "/run/user/1000");
        f.programs.insert(
            PathBuf::from(CRUN),
            Ok((true, "crun version 1.14.1\ncommit: abc\n".to_owned())),
        );
        f
    }
}

impl Host for Fake {
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        if let Some(kind) = self.stat_errors.get(path) {
            return Err(io::Error::from(*kind));
        }
        self.files
            .get(path)
            .map(|(s, _)| *s)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.stat(path)?.kind != Kind::File {
            // As `read` of a directory fails.
            return Err(io::Error::other("is a directory"));
        }
        Ok(self.files[path].1.clone())
    }

    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn euid(&self) -> u32 {
        self.euid
    }

    fn nofile(&self) -> u64 {
        self.nofile
    }

    fn run(&self, program: &Path, _: &[&str]) -> io::Result<Output> {
        match self.programs.get(program) {
            Some(Ok((success, stdout))) => Ok(Output {
                success: *success,
                stdout: stdout.clone(),
            }),
            Some(Err(kind)) => Err(io::Error::from(*kind)),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

// ---- certificates as the farm's issuer makes them -----------------------------------------

type Ca = rcgen::CertifiedIssuer<'static, rcgen::KeyPair>;

fn ca() -> Ca {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().expect("key"))
        .expect("CA")
}

/// A date as (year, month, day).
type Ymd = (i32, u8, u8);

struct Leaf {
    role: Option<&'static str>,
    ekus: Vec<Eku>,
    dns: Vec<&'static str>,
    /// (year, month, day) of not-before and not-after.
    validity: Option<(Ymd, Ymd)>,
}

impl Leaf {
    /// The daemon's row: role daemon, clientAuth, the one name `node-1`.
    fn daemon() -> Self {
        Self {
            role: Some("daemon"),
            ekus: vec![Eku::ClientAuth],
            dns: vec!["node-1"],
            validity: None,
        }
    }

    /// The certificate and its private key, both PEM.
    fn issue(&self, ca: &Ca) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(
            self.dns.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>(),
        )
        .expect("params");
        if let Some(role) = self.role {
            params.subject_alt_names.push(SanType::URI(
                format!("urn:kbf:role:{role}").try_into().expect("IA5"),
            ));
        }
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = self.ekus.clone();
        params.use_authority_key_identifier_extension = true;
        if let Some((from, to)) = self.validity {
            params.not_before = rcgen::date_time_ymd(from.0, from.1, from.2);
            params.not_after = rcgen::date_time_ymd(to.0, to.1, to.2);
        }
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, ca).expect("signed").pem();
        (cert, key.serialize_pem())
    }
}

// ---- the needs of a daemon ----------------------------------------------------------------

fn file(flag: &'static str, path: &str, secret: bool) -> FileNeed {
    FileNeed {
        flag,
        path: PathBuf::from(path),
        secret,
    }
}

fn daemon_needs() -> Needs {
    Needs {
        mode: "daemon",
        files: vec![
            file("--ca-cert", CA, false),
            file("--cert", CERT, false),
            file("--key", KEY, true),
        ],
        certs: vec![CertNeed {
            flag: "--cert",
            path: PathBuf::from(CERT),
            row: Row::Daemon,
            key: Some(("--key", PathBuf::from(KEY))),
            ca: Some(("--ca-cert", PathBuf::from(CA))),
            node_id: Some("node-1".to_owned()),
        }],
        s3: false,
        meta_and_data: None,
        container: Some(ContainerNeed {
            oci_runtime: Some(PathBuf::from(CRUN)),
        }),
    }
}

fn healthy() -> (Needs, Fake) {
    let ca = ca();
    let (cert, key) = Leaf::daemon().issue(&ca);
    (daemon_needs(), Fake::healthy(&cert, &key, &ca.pem()))
}

/// The one problem `report` holds, which must say each of `words`.
fn only(report: &Report, class: Class, words: &[&str]) {
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    let p = &report.problems[0];
    assert_eq!(p.class, class, "{p:?}");
    let line = report.render("daemon");
    for w in words {
        assert!(line.contains(w), "{w:?} not in {line}");
    }
    assert!(
        !p.what.is_empty() && !p.why.is_empty() && !p.fix.is_empty(),
        "{p:?}"
    );
}

#[test]
fn a_healthy_daemon_passes_with_nothing_printed() {
    let (needs, host) = healthy();
    let report = check(&needs, &host);
    assert_eq!(report, Report::default());
    assert_eq!(report.exit_status(), None);
    assert_eq!(report.render("daemon"), "");
}

// ---- files and secrets --------------------------------------------------------------------

#[test]
fn a_missing_file_is_refused_naming_the_flag_the_path_and_the_fix() {
    let (needs, host) = healthy();
    let report = check(&needs, &host.without(CA));
    // The certificate's chain cannot be checked without the CA, and says nothing more:
    // the one problem is the missing file.
    only(
        &report,
        Class::Refused,
        &["--ca-cert", CA, "does not exist"],
    );
    assert_eq!(report.exit_status(), Some(REFUSED));
}

#[test]
fn a_directory_where_a_file_belongs_is_refused() {
    let (needs, host) = healthy();
    let host = host.put(CERT, Kind::Dir, ME, 0o755, b"");
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--cert", CERT, "not a regular file"],
    );
}

#[test]
fn a_key_group_readable_is_refused_with_the_chmod_that_fixes_it() {
    let (needs, host) = healthy();
    let host = host.chmod(KEY, ME, 0o640);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--key", KEY, "0640", "chmod 600"],
    );
}

#[test]
fn a_key_world_readable_is_refused() {
    let (needs, host) = healthy();
    let host = host.chmod(KEY, ME, 0o604);
    only(&check(&needs, &host), Class::Refused, &["0604"]);
}

#[test]
fn a_key_owner_only_passes_at_0600_and_at_0400() {
    for mode in [0o600, 0o400] {
        let (needs, host) = healthy();
        let host = host.chmod(KEY, ME, mode);
        assert_eq!(check(&needs, &host), Report::default(), "{mode:o}");
    }
}

#[test]
fn a_key_owned_by_another_user_is_refused_with_the_chown_that_fixes_it() {
    let (needs, host) = healthy();
    let host = host.chmod(KEY, 0, 0o600);
    only(
        &check(&needs, &host),
        Class::Refused,
        &[KEY, "uid 0", "uid 1000", "chown 1000"],
    );
}

#[test]
fn a_certificate_may_be_world_readable() {
    // Only the secrets are held to 0600: a certificate and a CA are public.
    let (needs, host) = healthy();
    let ca = host.files[Path::new(CA)].1.clone();
    let host = host.file(CA, 0o666, &ca);
    assert_eq!(check(&needs, &host), Report::default());
}

#[test]
fn a_file_system_that_answers_busy_or_timed_out_is_not_ready_not_refused() {
    for kind in [io::ErrorKind::TimedOut, io::ErrorKind::ResourceBusy] {
        let (needs, mut host) = healthy();
        host.stat_errors.insert(PathBuf::from(CA), kind);
        let report = check(&needs, &host);
        only(&report, Class::NotReady, &[CA]);
        assert_eq!(report.exit_status(), Some(NOT_READY), "{kind:?}");
    }
}

#[test]
fn a_file_that_cannot_be_statted_for_permission_is_refused() {
    let (needs, mut host) = healthy();
    host.stat_errors
        .insert(PathBuf::from(KEY), io::ErrorKind::PermissionDenied);
    only(
        &check(&needs, &host),
        Class::Refused,
        &[KEY, "permission denied"],
    );
}

// ---- certificates -------------------------------------------------------------------------

fn with_cert(leaf: &Leaf) -> (Needs, Fake) {
    let ca = ca();
    let (needs, _) = healthy();
    let (cert, key) = leaf.issue(&ca);
    (needs, Fake::healthy(&cert, &key, &ca.pem()))
}

#[test]
fn a_certificate_of_another_role_is_refused() {
    let mut leaf = Leaf::daemon();
    leaf.role = Some("storage");
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--cert", "urn:kbf:role:storage", "urn:kbf:role:daemon"],
    );
}

#[test]
fn a_certificate_without_a_role_is_refused() {
    let mut leaf = Leaf::daemon();
    leaf.role = None;
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["names no role", "urn:kbf:role:daemon"],
    );
}

#[test]
fn a_daemon_certificate_that_also_serves_is_refused_for_its_key_usage() {
    let mut leaf = Leaf::daemon();
    leaf.ekus = vec![Eku::ServerAuth, Eku::ClientAuth];
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["serverAuth,clientAuth", "not clientAuth"],
    );
}

#[test]
fn a_certificate_without_key_usage_is_refused() {
    let mut leaf = Leaf::daemon();
    leaf.ekus = vec![];
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["extended key usage is none"],
    );
}

#[test]
fn a_storage_certificate_must_carry_both_server_and_client_auth() {
    let ca = ca();
    let mut leaf = Leaf {
        role: Some("storage"),
        ekus: vec![Eku::ServerAuth, Eku::ClientAuth],
        dns: vec!["store.farm.example"],
        validity: None,
    };
    let mut needs = daemon_needs();
    needs.mode = "storage";
    needs.certs[0].row = Row::Storage;
    needs.certs[0].node_id = None;
    let (cert, key) = leaf.issue(&ca);
    let host = Fake::healthy(&cert, &key, &ca.pem());
    assert_eq!(check(&needs, &host), Report::default());
    leaf.ekus = vec![Eku::ServerAuth];
    let (cert, key) = leaf.issue(&ca);
    let host = Fake::healthy(&cert, &key, &ca.pem());
    only(
        &check(&needs, &host),
        Class::Refused,
        &["extended key usage is serverAuth"],
    );
}

#[test]
fn a_daemon_certificate_that_names_another_node_is_refused() {
    let mut leaf = Leaf::daemon();
    leaf.dns = vec!["node-2"];
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["node-2", "node-1", "--node-id"],
    );
}

#[test]
fn a_daemon_certificate_that_names_more_than_its_node_is_refused() {
    let mut leaf = Leaf::daemon();
    leaf.dns = vec!["node-1", "node-2"];
    let (needs, host) = with_cert(&leaf);
    only(
        &check(&needs, &host),
        Class::Refused,
        &["node-1 node-2", "exactly"],
    );
}

#[test]
fn a_certificate_from_another_ca_is_refused() {
    let leaf = Leaf::daemon();
    let other = ca();
    let (needs, host) = healthy();
    let (cert, key) = leaf.issue(&other);
    let host = host
        .file(CERT, 0o644, cert.as_bytes())
        .file(KEY, 0o600, key.as_bytes());
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--cert", "--ca-cert", "does not chain"],
    );
}

#[test]
fn a_key_that_is_not_the_certificates_is_refused_naming_both_flags() {
    let (needs, host) = healthy();
    let other = Leaf::daemon().issue(&ca()).1;
    let host = host.file(KEY, 0o600, other.as_bytes());
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--key", "--cert", "is not the private key"],
    );
}

#[test]
fn a_key_file_that_is_not_pem_is_refused() {
    let (needs, host) = healthy();
    let host = host.file(KEY, 0o600, b"key");
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--key", "no PEM private key"],
    );
}

#[test]
fn a_pem_private_key_of_a_kind_tls_cannot_sign_with_is_refused() {
    let (needs, host) = healthy();
    let bogus = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
    let host = host.file(KEY, 0o600, bogus.as_bytes());
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--key", "cannot sign with"],
    );
}

#[test]
fn a_certificate_naming_no_key_is_not_paired() {
    let (mut needs, host) = healthy();
    needs.certs[0].key = None;
    let host = host.file(KEY, 0o600, b"key");
    assert_eq!(check(&needs, &host), Report::default());
}

#[test]
fn a_certificate_with_garbage_in_it_is_refused() {
    let (needs, host) = healthy();
    let host = host.file(CERT, 0o644, b"not a certificate");
    only(
        &check(&needs, &host),
        Class::Refused,
        &["--cert", "no certificate"],
    );
}

#[test]
fn an_expired_certificate_warns_and_does_not_stop_the_start() {
    // The first load of an expired certificate starts on purpose (kbf-tls reloads the
    // renewed one); the self-check says so out loud instead.
    let mut leaf = Leaf::daemon();
    leaf.validity = Some(((2020, 1, 1), (2021, 1, 1)));
    let (needs, host) = with_cert(&leaf);
    let report = check(&needs, &host);
    assert!(report.problems.is_empty(), "{report:#?}");
    assert_eq!(report.exit_status(), None);
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(report.warnings[0].contains("expired"), "{report:#?}");
    assert!(report.render("daemon").starts_with("kbf daemon: warning: "));
}

#[test]
fn a_certificate_not_yet_valid_is_not_ready() {
    // A clock behind the issuer's heals; a restart may succeed.
    let mut leaf = Leaf::daemon();
    leaf.validity = Some(((3000, 1, 1), (3001, 1, 1)));
    let (needs, host) = with_cert(&leaf);
    let report = check(&needs, &host);
    only(&report, Class::NotReady, &["not valid yet"]);
    assert_eq!(report.exit_status(), Some(NOT_READY));
}

#[test]
fn a_certificate_without_a_ca_to_chain_to_is_checked_for_role_and_key_usage_only() {
    let mut leaf = Leaf::daemon();
    leaf.role = Some("storage");
    let other = ca();
    let (mut needs, host) = healthy();
    needs.certs[0].ca = None;
    let (cert, key) = leaf.issue(&other);
    let host = host
        .file(CERT, 0o644, cert.as_bytes())
        .file(KEY, 0o600, key.as_bytes());
    only(
        &check(&needs, &host),
        Class::Refused,
        &["urn:kbf:role:storage"],
    );
}

// ---- environment ----------------------------------------------------------------------------

fn s3_needs() -> Needs {
    Needs {
        mode: "storage",
        files: vec![],
        certs: vec![],
        s3: true,
        meta_and_data: None,
        container: None,
    }
}

#[test]
fn a_missing_s3_key_pair_is_two_problems_naming_each_variable() {
    let report = check(&s3_needs(), &Fake::empty());
    assert_eq!(report.problems.len(), 2, "{report:#?}");
    let text = report.render("storage");
    assert!(text.contains("AWS_ACCESS_KEY_ID"), "{text}");
    assert!(text.contains("AWS_SECRET_ACCESS_KEY"), "{text}");
    assert!(text.contains("EnvironmentFile"), "{text}");
    assert_eq!(report.exit_status(), Some(REFUSED));
}

#[test]
fn an_empty_s3_key_counts_as_missing() {
    let host = Fake::empty()
        .var("AWS_ACCESS_KEY_ID", "")
        .var("AWS_SECRET_ACCESS_KEY", "secret");
    let report = check(&s3_needs(), &host);
    only(&report, Class::Refused, &["AWS_ACCESS_KEY_ID", "empty"]);
}

#[test]
fn a_complete_s3_key_pair_passes() {
    let host = Fake::empty()
        .var("AWS_ACCESS_KEY_ID", "id")
        .var("AWS_SECRET_ACCESS_KEY", "secret");
    assert_eq!(check(&s3_needs(), &host), Report::default());
}

#[test]
fn no_s3_cold_store_needs_no_key_pair() {
    let mut needs = s3_needs();
    needs.s3 = false;
    assert_eq!(check(&needs, &Fake::empty()), Report::default());
}

fn meta_needs(meta: &str, data: &str) -> Needs {
    let mut needs = s3_needs();
    needs.s3 = false;
    needs.meta_and_data = Some((PathBuf::from(meta), PathBuf::from(data)));
    needs
}

#[test]
fn a_meta_dir_inside_the_data_dir_is_refused() {
    for meta in ["/var/lib/kbf/meta", "/var/lib/kbf", "/var/lib/kbf/a/b"] {
        let report = check(&meta_needs(meta, "/var/lib/kbf"), &Fake::empty());
        only(
            &report,
            Class::Refused,
            &["--meta-dir", meta, "--data-dir /var/lib/kbf"],
        );
    }
}

#[test]
fn a_meta_dir_beside_the_data_dir_passes() {
    // A shared name prefix is not a shared directory.
    for meta in ["/var/lib/kbf-meta", "/srv/meta", "/var/lib"] {
        let report = check(&meta_needs(meta, "/var/lib/kbf"), &Fake::empty());
        assert_eq!(report, Report::default(), "{meta}");
    }
}

fn container_needs(oci: Option<&str>) -> Needs {
    let mut needs = s3_needs();
    needs.s3 = false;
    needs.container = Some(ContainerNeed {
        oci_runtime: oci.map(PathBuf::from),
    });
    needs
}

fn container_host() -> Fake {
    let (_, host) = healthy();
    host
}

#[test]
fn podman_missing_from_the_path_is_refused() {
    let host = container_host().without("/usr/bin/podman");
    only(
        &check(&container_needs(None), &host),
        Class::Refused,
        &["podman", "PATH", "/usr/local/bin:/usr/bin"],
    );
}

#[test]
fn podman_found_on_a_later_path_entry_passes() {
    let host = container_host()
        .without("/usr/bin/podman")
        .file("/opt/podman/bin/podman", 0o755, b"")
        .var("PATH", "/usr/local/bin:/opt/podman/bin");
    assert_eq!(check(&container_needs(None), &host), Report::default());
}

#[test]
fn podman_that_is_not_executable_is_not_found() {
    let host = container_host().file("/usr/bin/podman", 0o644, b"");
    only(
        &check(&container_needs(None), &host),
        Class::Refused,
        &["podman", "PATH"],
    );
}

#[test]
fn a_missing_runtime_directory_variable_is_refused_with_the_linger_fix() {
    let mut host = container_host();
    host.vars.remove("XDG_RUNTIME_DIR");
    only(
        &check(&container_needs(None), &host),
        Class::Refused,
        &["XDG_RUNTIME_DIR", "loginctl enable-linger"],
    );
}

#[test]
fn a_runtime_directory_that_does_not_exist_is_refused_with_the_linger_fix() {
    let host = container_host().without("/run/user/1000");
    only(
        &check(&container_needs(None), &host),
        Class::Refused,
        &["/run/user/1000", "does not exist", "loginctl enable-linger"],
    );
}

#[test]
fn a_runtime_directory_of_another_user_is_refused() {
    let host = container_host().put("/run/user/1000", Kind::Dir, 0, 0o700, b"");
    only(
        &check(&container_needs(None), &host),
        Class::Refused,
        &["owned by uid 0", "uid 1000"],
    );
}

#[test]
fn a_crun_that_is_not_there_is_refused() {
    let host = container_host().without(CRUN);
    only(
        &check(&container_needs(Some(CRUN)), &host),
        Class::Refused,
        &[CRUN, "--oci-runtime", "install"],
    );
}

#[test]
fn a_crun_without_the_execute_bit_is_refused() {
    let host = container_host().file(CRUN, 0o644, b"");
    only(
        &check(&container_needs(Some(CRUN)), &host),
        Class::Refused,
        &[CRUN, "chmod +x"],
    );
}

#[test]
fn a_crun_that_will_not_report_its_version_is_refused() {
    let mut host = container_host();
    host.programs
        .insert(PathBuf::from(CRUN), Err(io::ErrorKind::PermissionDenied));
    only(
        &check(&container_needs(Some(CRUN)), &host),
        Class::Refused,
        &[CRUN, "--version"],
    );
    let mut host = container_host();
    host.programs
        .insert(PathBuf::from(CRUN), Ok((false, String::new())));
    only(
        &check(&container_needs(Some(CRUN)), &host),
        Class::Refused,
        &[CRUN, "--version"],
    );
}

#[test]
fn a_crun_older_than_the_minimum_is_refused_naming_both_versions() {
    for old in [
        "crun version 1.13.9\n",
        "crun version 0.9\n",
        "crun version 1.8.4\n",
    ] {
        let mut host = container_host();
        host.programs
            .insert(PathBuf::from(CRUN), Ok((true, old.to_owned())));
        only(
            &check(&container_needs(Some(CRUN)), &host),
            Class::Refused,
            &["1.14"],
        );
    }
}

#[test]
fn a_crun_at_or_above_the_minimum_passes() {
    for good in [
        "crun version 1.14\n",
        "crun version 1.14.1\ncommit: x\n",
        "crun version 1.20.0\n",
        "crun version 2.0.1\n",
    ] {
        let mut host = container_host();
        host.programs
            .insert(PathBuf::from(CRUN), Ok((true, good.to_owned())));
        assert_eq!(
            check(&container_needs(Some(CRUN)), &host),
            Report::default(),
            "{good}"
        );
    }
}

#[test]
fn a_crun_whose_version_cannot_be_read_is_refused() {
    let mut host = container_host();
    host.programs
        .insert(PathBuf::from(CRUN), Ok((true, "hello\n".to_owned())));
    only(
        &check(&container_needs(Some(CRUN)), &host),
        Class::Refused,
        &["hello"],
    );
}

#[test]
fn podman_as_the_runtime_asks_nothing_of_crun() {
    let host = container_host().without(CRUN);
    assert_eq!(check(&container_needs(None), &host), Report::default());
}

#[test]
fn a_low_open_file_limit_warns_with_the_number_and_does_not_stop_the_start() {
    let mut host = Fake::empty();
    host.nofile = 1024;
    let report = check(&s3_needs_without_s3(), &host);
    assert!(report.problems.is_empty());
    assert_eq!(report.exit_status(), None);
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(report.warnings[0].contains("1024"), "{report:#?}");
    assert!(report.warnings[0].contains("LimitNOFILE"), "{report:#?}");
}

#[test]
fn the_open_file_floor_itself_does_not_warn() {
    let mut host = Fake::empty();
    host.nofile = super::NOFILE_FLOOR;
    assert_eq!(check(&s3_needs_without_s3(), &host), Report::default());
    host.nofile = super::NOFILE_FLOOR - 1;
    assert_eq!(check(&s3_needs_without_s3(), &host).warnings.len(), 1);
}

fn s3_needs_without_s3() -> Needs {
    let mut n = s3_needs();
    n.s3 = false;
    n
}

// ---- all together ---------------------------------------------------------------------------

#[test]
fn every_failure_is_reported_together_one_line_each_and_the_start_is_refused() {
    let (mut needs, host) = healthy();
    needs.s3 = true;
    needs.meta_and_data = Some((
        PathBuf::from("/var/lib/kbf/meta"),
        PathBuf::from("/var/lib/kbf"),
    ));
    let host = host
        .chmod(KEY, ME, 0o644)
        .without(CA)
        .without("/usr/bin/podman")
        .without(CRUN);
    let report = check(&needs, &host);
    let text = report.render("daemon");
    // The CA file, the key's mode, the S3 pair (2), the meta dir, podman, crun.
    assert_eq!(report.problems.len(), 7, "{text}");
    assert_eq!(text.lines().count(), 7, "{text}");
    for line in text.lines() {
        assert!(line.starts_with("kbf daemon: "), "{line}");
    }
    for word in [
        "--ca-cert",
        "0644",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "--meta-dir",
        "podman",
        CRUN,
    ] {
        assert!(text.contains(word), "{word} not in {text}");
    }
    assert_eq!(report.exit_status(), Some(REFUSED));
}

#[test]
fn a_not_ready_problem_beside_a_refused_one_exits_refused() {
    let (needs, mut host) = healthy();
    host.stat_errors
        .insert(PathBuf::from(CA), io::ErrorKind::TimedOut);
    let host = host.chmod(KEY, ME, 0o644);
    let report = check(&needs, &host);
    assert_eq!(report.problems.len(), 2, "{report:#?}");
    assert_eq!(report.exit_status(), Some(REFUSED));
}

#[test]
fn the_exit_statuses_are_the_ones_the_units_and_the_storage_classifier_use() {
    // RestartPreventExitStatus=2 in every unit; 75 is EX_TEMPFAIL (kbf-server's
    // start_exit::NOT_READY).
    assert_eq!(REFUSED, 2);
    assert_eq!(NOT_READY, 75);
}

// ---- the flags of each mode reach the checks ----------------------------------------------

mod wiring {
    use std::path::PathBuf;

    use super::super::{CertNeed, Needs};
    use super::super::{Row, needs};

    fn parse(args: &[&str]) -> Needs {
        let argv = std::iter::once("kbf").chain(args.iter().copied());
        needs(&crate::parse_from(argv).expect("flags parse"))
    }

    fn flags_of(n: &Needs) -> Vec<(&'static str, bool)> {
        n.files.iter().map(|f| (f.flag, f.secret)).collect()
    }

    fn cert<'a>(n: &'a Needs, flag: &str) -> &'a CertNeed {
        n.certs
            .iter()
            .find(|c| c.flag == flag)
            .unwrap_or_else(|| panic!("no certificate {flag} in {:?}", n.certs))
    }

    const DAEMON: [&str; 9] = [
        "--server=https://front:7070",
        "--ca-cert=/c/ca.pem",
        "--cert=/c/node.pem",
        "--key=/c/node.key",
        "--node-id=node-1",
        "--driver=container",
        "--container-runtime=oci",
        "--oci-runtime=/opt/crun",
        "--data-dir=/var/lib/kbf",
    ];

    fn with(mode: &'static str, own: &[&'static str], rest: &[&'static str]) -> Needs {
        let all: Vec<&str> = std::iter::once(mode)
            .chain(own.iter().copied())
            .chain(rest.iter().copied())
            .collect();
        parse(&all)
    }

    #[test]
    fn a_daemon_needs_its_three_files_its_row_and_the_container_programs() {
        let n = with("daemon", &DAEMON, &[]);
        assert_eq!(n.mode, "daemon");
        assert_eq!(
            flags_of(&n),
            [("--cert", false), ("--ca-cert", false), ("--key", true)]
        );
        let c = cert(&n, "--cert");
        assert_eq!(c.row, Row::Daemon);
        assert_eq!(c.node_id.as_deref(), Some("node-1"));
        assert_eq!(c.key, Some(("--key", PathBuf::from("/c/node.key"))));
        assert_eq!(c.ca, Some(("--ca-cert", PathBuf::from("/c/ca.pem"))));
        assert_eq!(
            n.container.expect("container").oci_runtime,
            Some(PathBuf::from("/opt/crun"))
        );
        assert!(!n.s3 && n.meta_and_data.is_none());
    }

    #[test]
    fn a_daemon_on_podman_asks_nothing_of_crun_and_a_fake_one_asks_nothing() {
        let podman: Vec<&str> = DAEMON
            .iter()
            .copied()
            .filter(|f| !f.starts_with("--container-runtime") && !f.starts_with("--oci-runtime"))
            .collect();
        let n = with("daemon", &podman, &[]);
        assert_eq!(n.container.expect("container").oci_runtime, None);
        let fake: Vec<&str> = podman
            .iter()
            .map(|f| {
                if *f == "--driver=container" {
                    "--driver=fake"
                } else {
                    f
                }
            })
            .collect();
        assert_eq!(with("daemon", &fake, &[]).container, None);
    }

    const STORAGE: [&str; 6] = [
        "--cold-store=s3://bucket/p?endpoint=http://s3.example:9000",
        "--hot-store=file:///var/lib/kbf-hot",
        "--meta-dir=/var/lib/kbf/meta",
        "--storage-tls-cert=/c/storage.pem",
        "--storage-tls-key=/c/storage.key",
        "--storage-client-ca=/c/ca.pem",
    ];

    #[test]
    fn a_storage_node_needs_its_row_its_key_private_and_the_s3_pair() {
        let n = with("storage", &STORAGE, &[]);
        let c = cert(&n, "--storage-tls-cert");
        assert_eq!(c.row, Row::Storage);
        assert_eq!(
            c.key,
            Some(("--storage-tls-key", PathBuf::from("/c/storage.key")))
        );
        assert_eq!(
            c.ca,
            Some(("--storage-client-ca", PathBuf::from("/c/ca.pem")))
        );
        assert!(
            n.files
                .iter()
                .any(|f| f.flag == "--storage-tls-key" && f.secret)
        );
        assert!(n.s3);
        assert!(n.container.is_none() && n.meta_and_data.is_none());
    }

    #[test]
    fn a_file_cold_store_needs_no_s3_pair() {
        let file: Vec<&str> = STORAGE
            .iter()
            .map(|f| {
                if f.starts_with("--cold-store") {
                    "--cold-store=file:///var/lib/kbf-cold"
                } else {
                    f
                }
            })
            .collect();
        assert!(!with("storage", &file, &[]).s3);
    }

    #[test]
    fn storage_and_daemon_needs_both_halves_and_compares_the_meta_dir_with_the_data_dir() {
        let n = with("storage-and-daemon", &STORAGE, &DAEMON);
        assert_eq!(cert(&n, "--storage-tls-cert").row, Row::Storage);
        assert_eq!(cert(&n, "--cert").row, Row::Daemon);
        assert!(n.s3);
        assert!(n.container.is_some());
        assert_eq!(
            n.meta_and_data,
            Some((
                PathBuf::from("/var/lib/kbf/meta"),
                PathBuf::from("/var/lib/kbf")
            ))
        );
    }

    #[test]
    fn a_frontend_needs_its_listener_files_and_its_client_certificate_toward_storage() {
        let n = parse(&[
            "frontend",
            "--storage=https://store.example:8983",
            "--reapi-tls-cert=/c/reapi.pem",
            "--reapi-tls-key=/c/reapi.key",
            "--worker-tls-cert=/c/worker.pem",
            "--worker-tls-key=/c/worker.key",
            "--worker-client-ca=/c/ca.pem",
            "--worker-deny-list=/c/deny",
            "--api-listen=127.0.0.1:8990",
            "--api-token-file=/c/token",
            "--storage-client-cert=/c/fe.pem",
            "--storage-client-key=/c/fe.key",
            "--storage-ca=/c/ca.pem",
        ]);
        assert_eq!(n.mode, "frontend");
        let secrets: Vec<&str> = n
            .files
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.flag)
            .collect();
        assert_eq!(
            secrets,
            [
                "--reapi-tls-key",
                "--api-token-file",
                "--worker-tls-key",
                "--storage-client-key"
            ]
        );
        assert!(
            n.files
                .iter()
                .any(|f| f.flag == "--worker-deny-list" && !f.secret)
        );
        // The worker certificate chains to the worker client CA; the client certificate
        // toward storage has no CA of the mode's to chain to.
        let worker = cert(&n, "--worker-tls-cert");
        assert_eq!(worker.row, Row::Frontend);
        assert_eq!(
            worker.ca,
            Some(("--worker-client-ca", PathBuf::from("/c/ca.pem")))
        );
        assert_eq!(
            worker.key,
            Some(("--worker-tls-key", PathBuf::from("/c/worker.key")))
        );
        assert_eq!(cert(&n, "--storage-client-cert").ca, None);
        assert_eq!(
            cert(&n, "--storage-client-cert").key,
            Some(("--storage-client-key", PathBuf::from("/c/fe.key")))
        );
        // The REAPI certificate is the public name's, not a row of the farm's table.
        assert!(n.certs.iter().all(|c| c.flag != "--reapi-tls-cert"));
        assert!(n.files.iter().any(|f| f.flag == "--reapi-tls-cert"));
        assert!(!n.s3 && n.container.is_none());
    }

    #[test]
    fn a_plaintext_loopback_mode_needs_no_files() {
        let n = parse(&["frontend", "--storage=http://127.0.0.1:8983"]);
        assert!(n.files.is_empty() && n.certs.is_empty(), "{n:?}");
    }
}
