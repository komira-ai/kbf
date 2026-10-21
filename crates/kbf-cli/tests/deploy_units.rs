//! The units kbf ships (`packaging/linux/units/`, installed by kbf-installer) against this
//! binary's own parser, in-process, with the `kbf.toml` each mode is started with: the
//! package's unit is `kbf --mode=<mode>` and nothing else, every setting is the file's.
//! The file is the one `kbf-installer setup` writes for the mode
//! (`kbf-installer/tests/golden/kbf_toml`), or, for a storage node an operator wrote by
//! hand, the stores and certificates in a file of its own.
//!
//! Where a defect class is already caught elsewhere, the test is named here, not
//! repeated: the files of the package are exactly kbf's modes
//! (`package_units.rs::the_package_has_a_unit_for_each_kbf_mode`), each unit's `--check`
//! and start lines parse (`package_units.rs`), a key kbf refuses in `kbf.toml` is refused
//! naming the key and the file (`config_file.rs::an_unknown_key_is_refused_naming_the_key_and_the_file`),
//! every key `setup` writes is a real flag of its table
//! (`kbf-installer`'s `kbf_settings_tests`), and the preflight accepts a metadata log
//! under the node's data root for a storage-and-daemon node
//! (`preflight/tests.rs::a_storage_and_daemon_node_with_its_meta_dir_under_its_data_dir_passes`).
//!
//! The daemon, storage and storage-and-daemon units are a start that enrolls: their
//! certificate files are the enrolled store's, by role, and the flags they run with are
//! `node::plan`'s, held by `src/node_tests.rs`
//! (`a_daemon_only_node_runs_kbf_daemon_with_the_frontends_endpoints`,
//! `a_storage_only_node_runs_kbf_storage_registered_with_the_frontend` and
//! `a_node_with_both_roles_runs_the_converged_mode`, which names each half's pair and CA).

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use kbf_cli::names::MODES;
use kbf_cli::{Invocation, parse_with};
use kbf_testkit::dir::TestDir;
use kbf_unitfile::UnitFile;

/// The program every packaged unit runs: the release `kbf-installer` flipped to.
const PROGRAM: &str = "/var/lib/kbf-installer/releases/current/kbf";

/// The `kbf.toml` `kbf-installer setup` writes for `mode`.
fn setup_file(mode: &str) -> &'static str {
    match mode {
        "all-in-one" => {
            include_str!("../../kbf-installer/tests/golden/kbf_toml/all-in-one.toml")
        }
        "daemon" => include_str!("../../kbf-installer/tests/golden/kbf_toml/daemon.toml"),
        "frontend" => include_str!("../../kbf-installer/tests/golden/kbf_toml/frontend.toml"),
        "storage" => include_str!("../../kbf-installer/tests/golden/kbf_toml/storage.toml"),
        "storage-and-daemon" => {
            include_str!("../../kbf-installer/tests/golden/kbf_toml/storage-and-daemon.toml")
        }
        other => panic!("no setup file for the mode {other}"),
    }
}

/// A `storage` node an operator configured by hand, without enrollment: its listener,
/// certificates and stores are the file's, as the unit the package ships gives none.
const STORAGE_BY_HAND: &str = "\
[listener]
storage-listen = \"192.0.2.10:8983\"

[tls]
storage-tls-cert = \"/etc/kbf/storage.pem\"
storage-tls-key = \"/etc/kbf/storage.key\"
storage-client-ca = \"/etc/kbf/ca.pem\"

[stores]
cold-store = \"file:///var/lib/kbf-server/cold\"
hot-store = \"file:///var/lib/kbf-server/hot\"
meta-dir = \"/var/lib/kbf-server/raft\"
";

/// The packaged unit file `name`, parsed.
fn unit(name: &str) -> UnitFile {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/linux/units")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    UnitFile::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The words after the program of the unit's `ExecStart=`, asserting the program is the
/// installer's current release. The plain frontend unit names the host's file
/// (`--config /etc/kbf/kbf.toml`); that pair is dropped, as the test gives its own.
fn start_words(unit: &UnitFile, name: &str) -> Vec<String> {
    let argv = unit.exec_start();
    assert_eq!(argv.first().copied(), Some(PROGRAM), "{name}: {argv:?}");
    let mut words = Vec::new();
    let mut rest = argv[1..].iter();
    while let Some(w) = rest.next() {
        if *w == "--config" {
            assert_eq!(rest.next().copied(), Some("/etc/kbf/kbf.toml"), "{name}");
        } else {
            words.push((*w).to_owned());
        }
    }
    words
}

/// `kbf`'s argv for these words and the file the host's `/etc/kbf/kbf.toml` stands for.
fn argv(words: &[String], config: &Path) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec!["kbf".into()];
    out.extend(words.iter().map(OsString::from));
    out.extend(["--config".into(), config.into()]);
    out
}

/// `text` as `kbf.toml` in `dir`, mode 0600 as `setup` leaves it.
fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("kbf.toml");
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

/// The database `setup` writes for a host that keeps its state in Postgres, as kbf's own
/// URI parser reads it: the socket of the OS package, the role of the server user, no password.
fn setup_state_store() -> kbf_server::StateStoreUri {
    "postgres:///kbf?host=/run/postgresql&user=kbf-server"
        .parse()
        .expect("a store URI")
}

fn paths(p: [PathBuf; 3]) -> [String; 3] {
    p.map(|p| p.display().to_string())
}

/// Catches a packaged unit that starts another mode than its file is named after (a
/// `--mode=daemon` in `kbf-storage.service`), a unit that gives kbf a flag of its own (the
/// settings are the file's), and a `kbf.toml` that `setup` writes whose values kbf reads
/// as other settings than the ones written. Each mode's unit and its setup file are read
/// by kbf's own parser: the frontend and the all-in-one resolve to their flags (listeners,
/// certificates toward storage, token file and the Postgres state store as the file
/// spells them, no handover flag, which a Postgres store refuses), and the three modes
/// that enroll resolve to a start of the mode the unit names, on the file.
#[test]
fn each_packaged_mode_unit_starts_its_own_mode_on_the_kbf_toml_setup_writes() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "packaged-mode-units");
    for mode in MODES {
        let name = format!("kbf-{}.service", mode.name);
        let words = start_words(&unit(&name), &name);
        assert_eq!(words, [format!("--mode={}", mode.name)], "{name}");
        let config = write_config(dir.path(), setup_file(mode.name));
        let parsed = match parse_with(argv(&words, &config), None) {
            Ok(parsed) => parsed,
            Err(e) => panic!("kbf {} refuses {name} on its setup file:\n{e}", mode.name),
        };
        assert!(!parsed.check, "{name} is a start");
        match parsed.invocation {
            Invocation::Frontend(flags) => {
                assert_eq!(mode.name, "frontend");
                assert_eq!(
                    flags.storage.uri().to_string(),
                    "https://storage.example:8983/"
                );
                let tls = flags.tls.expect("mutual TLS toward storage");
                assert_eq!(
                    paths(tls),
                    [
                        "/etc/kbf/worker.pem",
                        "/etc/kbf/worker.key",
                        "/etc/kbf/ca.pem"
                    ]
                );
                assert_eq!(flags.server.listen.to_string(), "0.0.0.0:8980");
                assert_eq!(flags.server.worker_listen.to_string(), "0.0.0.0:8981");
                assert_eq!(
                    flags.server.api_listen.map(|a| a.to_string()).as_deref(),
                    Some("127.0.0.1:8982")
                );
                // The operator API's token is the file the setup names, so a write is not 401.
                assert_eq!(
                    flags.server.api_token_file.as_deref(),
                    Some(Path::new("/etc/kbf/api-token"))
                );
                assert_eq!(flags.server.state_store, Some(setup_state_store()));
                // `kbf frontend` refuses a Postgres store with a control socket.
                assert!(
                    flags.handover.is_none(),
                    "the postgres file gives a handover"
                );
            }
            Invocation::AllInOne(flags) => {
                assert_eq!(mode.name, "all-in-one");
                assert_eq!(
                    flags.server.api_token_file.as_deref(),
                    Some(Path::new("/etc/kbf/api-token"))
                );
                assert_eq!(flags.server.state_store, Some(setup_state_store()));
                assert_eq!(
                    flags.server.meta_dir.as_deref(),
                    Some(Path::new("/var/lib/kbf-meta"))
                );
                assert!(flags.server.cold_store.is_some(), "no cold store");
            }
            Invocation::Registered(start) => {
                assert_eq!(start.mode, Some(mode.name), "{name} starts another mode");
                assert_eq!(start.config, config);
                assert!(!start.check);
            }
            other => panic!("{name} on its setup file is {other:?}"),
        }
    }
}

/// Catches the storage unit's settings read as others, or dropped, for a node an operator
/// configured by hand: the certificate flags are read as the paths they spell (not
/// dropped, and not another flag's), so the node serves with mutual TLS, and its
/// listener, hot and cold stores and metadata log are the file's.
#[test]
fn the_storage_unit_reads_a_hand_written_kbf_toml_for_its_certificates_and_stores() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "packaged-storage-by-hand");
    let words = start_words(&unit("kbf-storage.service"), "kbf-storage.service");
    let config = write_config(dir.path(), STORAGE_BY_HAND);
    let flags = match parse_with(argv(&words, &config), None).map(|p| p.invocation) {
        Ok(Invocation::Storage(flags)) => flags,
        other => panic!("not kbf storage: {other:?}"),
    };
    assert_eq!(
        paths(flags.tls.expect("the file turns on mutual TLS")),
        [
            "/etc/kbf/storage.pem",
            "/etc/kbf/storage.key",
            "/etc/kbf/ca.pem"
        ]
    );
    assert_eq!(flags.listen.to_string(), "192.0.2.10:8983");
    assert_eq!(
        flags.server.meta_dir.as_deref(),
        Some(Path::new("/var/lib/kbf-server/raft"))
    );
    assert!(flags.server.cold_store.is_some(), "no cold store");
    assert!(flags.server.hot_store.is_some(), "no hot store");
}

/// A host where every path exists as a file and a directory alike, empty and mode 0600.
struct EverythingExists;

impl kbf_cli::preflight::Host for EverythingExists {
    fn stat(&self, _: &std::path::Path) -> std::io::Result<kbf_cli::preflight::host::Stat> {
        Ok(kbf_cli::preflight::host::Stat {
            kind: kbf_cli::preflight::host::Kind::File,
            uid: 0,
            mode: 0o600,
        })
    }
    fn lstat(&self, path: &std::path::Path) -> std::io::Result<kbf_cli::preflight::host::Lstat> {
        Ok(kbf_cli::preflight::host::Lstat {
            stat: self.stat(path)?,
            gid: 0,
        })
    }
    fn read(&self, _: &std::path::Path) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn var(&self, _: &str) -> Option<String> {
        Some("x".to_owned())
    }
    fn var_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn euid(&self) -> u32 {
        0
    }
    fn egid(&self) -> u32 {
        0
    }
    fn groups(&self) -> Vec<u32> {
        Vec::new()
    }
    fn nofile(&self) -> u64 {
        1 << 20
    }
    fn run(
        &self,
        _: &std::path::Path,
        _: &[&str],
    ) -> std::io::Result<kbf_cli::preflight::host::Output> {
        Ok(kbf_cli::preflight::host::Output {
            success: true,
            stdout: "crun version 1.14.0\n".to_owned(),
        })
    }
    fn now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}

/// Catches a startup preflight rule refusing a layout the package's files give: each
/// packaged unit with its setup file (and the by-hand storage file), with the file's
/// metadata directory and with the test farm's (the metadata log under the node's own data
/// root), is read by `kbf`'s own parser and checked against a host where every file
/// exists. The files may still fail their own checks (these are empty); no problem may be
/// about the two directories.
#[test]
fn no_packaged_unit_is_refused_over_where_its_directories_are() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "packaged-unit-preflight");
    let mut cases: Vec<(&str, String)> = MODES
        .iter()
        .map(|m| (m.name, setup_file(m.name).to_owned()))
        .collect();
    cases.push(("storage", STORAGE_BY_HAND.to_owned()));
    let mut moved = 0;
    for (mode, text) in cases {
        let farm = text
            .lines()
            .map(|l| {
                if l.starts_with("meta-dir = ") {
                    moved += 1;
                    "meta-dir = \"/var/lib/kbf/av2-meta\""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        for text in [text.clone(), farm] {
            let name = format!("kbf-{mode}.service");
            let words = start_words(&unit(&name), &name);
            let config = write_config(dir.path(), &text);
            let invocation = parse_with(argv(&words, &config), None)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .invocation;
            let needs = kbf_cli::preflight::needs(&invocation);
            let report = kbf_cli::preflight::check(&needs, &EverythingExists);
            let text = report.render(needs.mode);
            assert!(
                !text.contains("--meta-dir") && !text.contains("--data-dir"),
                "{mode}: {text}"
            );
        }
    }
    assert!(moved >= 2, "no setup file names a metadata directory");
}

/// A certificate made for each row of the farm's certificate table, as the farm's
/// issuer makes it (an EC P-256 key; `CA:FALSE`, `digitalSignature`, the row's extended
/// key usage, the authority key id; its names and one role URI in the subjectAltName),
/// passes `kbf-deploy linux verify-cert` for its row, and names its role the way a
/// storage node reads it (kbf-storage's `role_of`, the admission of every gRPC call).
/// Catches the two spellings drifting apart (the `urn:kbf:role:` prefix changed on one
/// side only, a role spelled otherwise or missing from the table), and a row whose
/// certificate verify-cert passes but storage reads as another role or none.
#[test]
fn each_rows_certificate_names_its_role_as_storage_reads_it() {
    use kbf_ca::verify_cert::{CLIENT_AUTH, ROLE_URI_PREFIX, Row, verify};
    use kbf_storage::service::{PeerRole, role_of};
    use rcgen::{ExtendedKeyUsagePurpose as Eku, KeyUsagePurpose, SanType};

    assert_eq!(ROLE_URI_PREFIX, kbf_storage::service::ROLE_URI_PREFIX);
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let mut roles = Vec::new();
    for row in Row::ALL {
        let name = if row == Row::Daemon {
            "node-1"
        } else {
            "node-1.farm.example"
        };
        let mut leaf = rcgen::CertificateParams::new(vec![name.to_owned()]).expect("params");
        leaf.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{}", row.role())
                .try_into()
                .expect("IA5"),
        ));
        leaf.is_ca = rcgen::IsCa::ExplicitNoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = row
            .ekus()
            .iter()
            .map(|e| {
                if *e == CLIENT_AUTH {
                    Eku::ClientAuth
                } else {
                    Eku::ServerAuth
                }
            })
            .collect();
        leaf.use_authority_key_identifier_extension = true;
        let cert = leaf
            .signed_by(&rcgen::KeyPair::generate().expect("key"), &ca)
            .expect("signed");
        let node_id = (row == Row::Daemon).then_some(name);
        let failures = verify(
            ca.pem().as_bytes(),
            cert.pem().as_bytes(),
            row,
            node_id,
            std::time::SystemTime::now(),
        )
        .expect("checked");
        assert!(failures.is_empty(), "{row:?}: {failures:?}");
        let role = role_of(cert.der()).expect("a role storage admits");
        assert_eq!(role.name(), row.role(), "{row:?}");
        roles.push(role);
    }
    // Every role storage knows is some row's.
    for role in PeerRole::ALL {
        assert!(roles.contains(&role), "{role:?} is no row's");
    }
}

/// Catches `kbf` taking a flag an operator added to a packaged unit's `ExecStart=` that it
/// refuses: an unknown flag, a store flag in the daemon's unit (a build node holds no
/// store), and another mode's flag in the storage unit. The words with that flag are
/// refused by kbf's own parser, in-process, with the message `kbf --mode=<mode> --check`
/// prints; the unedited words parse (above).
#[test]
fn a_packaged_unit_edited_with_a_flag_kbf_refuses_is_refused_by_its_parser() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "packaged-unit-edited");
    let refused = |mode: &str, flag: &str| {
        let name = format!("kbf-{mode}.service");
        let mut words = start_words(&unit(&name), &name);
        words.push(flag.to_owned());
        let config = write_config(dir.path(), setup_file(mode));
        parse_with(argv(&words, &config), None).expect_err(flag)
    };
    let e = refused("daemon", "--bogus-flag");
    assert!(
        e.to_string().contains("unexpected argument '--bogus-flag'"),
        "{e}"
    );
    assert_eq!(e.exit_code(), 2);
    let e = refused("daemon", "--cold-store=file:///x");
    assert!(
        e.to_string().contains("kbf daemon refuses --cold-store"),
        "{e}"
    );
    assert_eq!(e.exit_code(), 2);
    let e = refused("storage", "--worker-listen=192.0.2.10:8981");
    assert!(
        e.to_string()
            .contains("kbf storage refuses --worker-listen"),
        "{e}"
    );
}

/// Catches `kbf frontend handover` starting instances the packaged template does not
/// have: kbf-cli spells the template's name and instances itself, and the package's must
/// be the same words. The template file is `<TEMPLATE>@.service` and its name is the
/// frontend mode's; its default instance is the first one a start with none up picks; its
/// start line, for each instance, is a command line kbf's parser takes (the file, with
/// the handover socket, stands for the frontend's settings).
#[test]
fn the_handover_command_starts_the_instances_of_the_frontends_template() {
    use kbf_cli::handover::{INSTANCES, TEMPLATE};
    assert_eq!(TEMPLATE, "kbf-frontend");
    let template = unit(&format!("{TEMPLATE}@.service"));
    assert_eq!(
        template.get_in("Install", "DefaultInstance"),
        Some(INSTANCES[0])
    );
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "packaged-handover");
    // The parse accepts a Postgres store beside a control socket, and the runtime refuses
    // the pairing (`HandoverError::PostgresStore`, in kbf-server's `state_uri`), so this
    // test does not say that the template and a Postgres store run together. It uses the
    // SQLite store variant, which `StateStoreUri` still has, so the file stands for a
    // frontend that hands over.
    let config = write_config(
        dir.path(),
        "[storage-client]\nstorage = \"http://127.0.0.1:1\"\n\n[scheduler]\nstate-store = \"sqlite:///var/lib/kbf-server/state.db\"\n",
    );
    for instance in INSTANCES {
        let argv = template.exec_start();
        let words: Vec<String> = argv[1..]
            .iter()
            .map(|w| w.replace("%i", instance))
            .collect();
        assert!(
            words.contains(&format!("--instance={instance}")),
            "{instance}: {words:?}"
        );
        match parse_with(self::argv(&words, &config), None).map(|p| p.invocation) {
            Ok(Invocation::Frontend(flags)) => assert!(flags.handover.is_some()),
            other => panic!("{instance}: not the frontend: {other:?}"),
        }
    }
}

/// The `[listener]` addresses of a `kbf.toml`, `(key, port)`.
fn listener_ports(text: &str) -> Vec<(String, u16)> {
    let doc: toml::Table = text.parse().expect("a kbf.toml");
    let Some(listener) = doc.get("listener").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    listener
        .iter()
        .filter(|(k, _)| k.ends_with("listen"))
        .map(|(k, v)| {
            let addr = v.as_str().expect("an address");
            let port = addr.rsplit(':').next().expect("a port");
            (k.clone(), port.parse().expect("a number"))
        })
        .collect()
}

/// The first two listener keys that share a port, if any.
fn shared_port(text: &str) -> Option<(String, String)> {
    let ports = listener_ports(text);
    ports.iter().enumerate().find_map(|(i, (a, p))| {
        ports[i + 1..]
            .iter()
            .find(|(_, q)| q == p)
            .map(|(b, _)| (a.clone(), b.clone()))
    })
}

/// Catches two listeners of one mode on one port (the retired `ports` rule of
/// `check-units`, now that the listeners are `kbf.toml`'s): every file `kbf-installer setup`
/// writes, and the by-hand storage file, gives each listener its own port. The planted file
/// is the mutant: the checker must see two listeners on one port, and there must be
/// listeners to check.
#[test]
fn no_two_listeners_of_a_mode_share_a_port() {
    let mut seen = 0;
    for mode in MODES {
        let text = setup_file(mode.name);
        seen += listener_ports(text).len();
        assert_eq!(shared_port(text), None, "{}", mode.name);
    }
    assert_eq!(shared_port(STORAGE_BY_HAND), None);
    assert!(seen >= 6, "{seen} listeners read: the files moved?");
    let planted = "[listener]\nlisten = \"0.0.0.0:8980\"\nworker-listen = \"0.0.0.0:8980\"\n";
    assert_eq!(
        shared_port(planted),
        Some(("listen".to_owned(), "worker-listen".to_owned()))
    );
}
