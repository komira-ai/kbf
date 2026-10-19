//! The root helper with its real release fetcher: an `activate` order is pulled through the
//! fetch child from a local HTTPS file server, verified by the stage pipeline (a fake `gh` and a
//! fake probe stand in for the two programs a scratch run cannot make root-owned), installed as
//! `releases/<commit>/` and flipped; a `frontend_update` locator (no components) is resolved
//! from the server's index and staged without a link moving.
//!
//! The helper's own passes run on a scratch root; the fetch child is the built binary, started
//! exactly as on a host (only the switch of user is left out, as these tests do not run as
//! root). Every test names the rule it holds and the mutant that turns it red.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::{FakeExec, ScratchHost};
use kbf_host::{Exec, Root};
use kbf_installer::fetch::release::ReleaseFetcher;
use kbf_installer::helper::cycle::{Config, Helper};
use kbf_installer::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::helper::restart::Restart;
use kbf_installer::helper::service::Procs;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::{Ran, Runner, Spawn};
use kbf_installer::state::{Locked, State};
use kbf_installer::testkit::{A, B, c};
use kbf_update::{Facts, Locator, OrderRecord, Verb};
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");
const T0: u64 = 1_800_000_000;
const UPDATE: &str = "var/lib/kbf-installer";
const MIB: usize = 1 << 20;

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn platform() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x86_64"
    }
}

/// The release a build of commit B would ship: a gzip tar holding one ELF named `kbf`.
fn tarball(payload: &str) -> Vec<u8> {
    let mut elf = vec![0_u8; 64];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[18] = if cfg!(target_arch = "aarch64") { 183 } else { 62 };
    elf.extend_from_slice(payload.as_bytes());
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(elf.len() as u64);
    header.set_mode(0o755);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(&mut header, "kbf", elf.as_slice())
        .expect("append");
    let tarred = builder.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tarred).expect("gzip");
    gz.finish().expect("gzip")
}

/// What `kbf --version --json` prints for commit B.
fn version_json() -> String {
    format!(
        "{{\"commit\":\"{}\",\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\
         \"state_schema\":{},\"version\":\"0.1.0\"}}\n",
        &B[..12],
        kbf_state::postgres::schema::latest()
    )
}

fn expected() -> Facts {
    Facts {
        platform: platform().to_owned(),
        protocol: 4,
        min_protocol: 3,
        state_schema: kbf_state::postgres::schema::latest(),
        profile: "release".to_owned(),
    }
}

/// What a bundle for `tarball` says: its digest as the subject. The fake `gh` reads it back.
fn good_bundle(tarball: &[u8]) -> String {
    format!("{{\"subject\":\"{}\"}}\n", sha(tarball))
}

// ----------------------------------------------------------------------------------- the PKI

struct Pki {
    ca: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
}

impl Pki {
    fn new() -> Pki {
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "kbf fetch wire test CA");
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let key = rcgen::KeyPair::generate().expect("key");
        Pki {
            ca: rcgen::CertifiedIssuer::self_signed(ca, key).expect("CA"),
        }
    }

    /// The certificate chain and key of a server named `localhost`.
    fn server(&self) -> (String, String) {
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }
}

// ------------------------------------------------------------------------------ the HTTPS server

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// A file server over TLS: what each path answers is set by the test, anything else is 404.
struct Https {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
}

impl Https {
    fn start(pki: &Pki) -> Https {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let (cert, key) = pki.server();
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert.as_bytes())
            .collect::<Result<_, _>>()
            .expect("chain");
        let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).expect("key");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("server config");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::default();
        let (seen, table) = (Arc::clone(&hits), Arc::clone(&routes));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (config, seen, table) =
                    (Arc::clone(&config), Arc::clone(&seen), Arc::clone(&table));
                std::thread::spawn(move || {
                    let _ = answer(stream, config, &seen, &table);
                });
            }
        });
        Https { addr, hits, routes }
    }

    fn set(&self, path: &str, status: u16, body: &[u8]) {
        self.routes.lock().expect("lock").insert(
            path.to_owned(),
            Reply {
                status,
                body: body.to_vec(),
            },
        );
    }

    fn url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("lock").clone()
    }
}

fn answer(
    mut tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    seen: &Mutex<Vec<String>>,
    routes: &Mutex<HashMap<String, Reply>>,
) -> io::Result<()> {
    let mut conn = rustls::ServerConnection::new(config).map_err(io::Error::other)?;
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if tls.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
    seen.lock().expect("lock").push(path.clone());
    let reply = routes
        .lock()
        .expect("lock")
        .get(&path)
        .cloned()
        .unwrap_or(Reply {
            status: 404,
            body: Vec::new(),
        });
    tls.write_all(
        format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.body.len()
        )
        .as_bytes(),
    )?;
    tls.write_all(&reply.body)?;
    tls.flush()?;
    tls.conn.send_close_notify();
    tls.flush()
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

// -------------------------------------------------------------------------- the stand-in programs

/// The programs the pipeline runs as the unprivileged user, played in this process: `gh`
/// accepts a bundle only if it names the digest of the file it is given and the commit asked
/// for; anything else is the probe, which prints commit B's `--version --json`.
#[derive(Default)]
struct Tools {
    calls: Mutex<Vec<Spawn>>,
}

impl Tools {
    fn programs(&self, name: &str) -> Vec<Spawn> {
        self.calls
            .lock()
            .expect("lock")
            .iter()
            .filter(|s| s.program.file_name().is_some_and(|n| n == name))
            .cloned()
            .collect()
    }
}

struct SharedTools(Arc<Tools>);

fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
    Ran {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        timed_out: false,
    }
}

impl Runner for SharedTools {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        self.0.calls.lock().expect("lock").push(spawn.clone());
        if !spawn.program.file_name().is_some_and(|n| n == "gh") {
            return Ok(ran(0, &version_json(), ""));
        }
        let arg = |i: usize| spawn.args[i].to_string_lossy().into_owned();
        let flag = |name: &str| {
            let at = spawn.args.iter().position(|a| a == name).expect("a flag");
            arg(at + 1)
        };
        let file = fs::read(arg(2))?;
        let bundle = fs::read_to_string(flag("--bundle"))?;
        if bundle.contains(&sha(&file)) && flag("--source-digest") == B {
            Ok(ran(0, "", ""))
        } else {
            Ok(ran(1, "", "the bundle does not attest this file"))
        }
    }
}

struct Idle;

impl LeaseSource for Idle {
    fn live(&self) -> Result<usize, String> {
        Ok(0)
    }
}

struct NoProcs;

impl Procs for NoProcs {
    fn exe_of(&self, _pid: u32) -> io::Result<PathBuf> {
        Err(io::ErrorKind::NotFound.into())
    }

    fn executables(&self) -> io::Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
}

// ------------------------------------------------------------------------------------- the rig

/// What a test chooses about the host's configuration.
struct Setup {
    /// `release_source`: the url of the server, or `frontend`.
    frontend_source: bool,
    /// The url names a port nothing listens on.
    down: bool,
    max_component_bytes: usize,
}

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            frontend_source: false,
            down: false,
            max_component_bytes: MIB,
        }
    }
}

struct Rig {
    root: PathBuf,
    layout: Layout,
    server: Https,
    tools: Arc<Tools>,
    setup: Setup,
    me: Owner,
}

fn root_entry(exec: &mut FakeExec, path: &str, dir: bool, bytes: &[u8]) {
    let owner = Owner { uid: 0, gid: 0 };
    let entry = if dir {
        Entry::Dir {
            perms: Perms::new(0o755),
            owner,
        }
    } else {
        Entry::File {
            perms: Perms::new(0o644),
            owner,
            bytes: bytes.to_vec(),
        }
    };
    exec.entries.insert(HostPath::new(path).expect("path"), entry);
}

impl Rig {
    /// A scratch host running release A with an empty spool, and a server.
    fn new(setup: Setup) -> (Rig, ScratchHost) {
        let host = ScratchHost::new("fetch-wire");
        let root = host.root.clone();
        for sub in [
            "run",
            "var/lib/kbf-installer/orders",
            "var/lib/kbf-installer/releases",
        ] {
            fs::create_dir_all(root.join(sub)).expect("seed");
        }
        let dir = root.join(UPDATE).join("releases").join(A);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        let layout = Layout::new(Root::new(&root).expect("absolute"), Platform::Linux);
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(A)),
                ..State::default()
            })
            .expect("state");
        let pki = Pki::new();
        let server = Https::start(&pki);
        fs::write(root.join("test-ca.pem"), pki.ca.pem()).expect("ca");
        let me = Owner {
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        };
        let rig = Rig {
            root,
            layout,
            server,
            tools: Arc::default(),
            setup,
            me,
        };
        (rig, host)
    }

    /// What the root-owned files say, as the trust checks read them.
    fn trust(&self) -> FakeExec {
        let mut exec = FakeExec::new(0);
        for dir in [
            "/usr",
            "/usr/share",
            "/usr/share/kbf-installer",
            "/usr/share/kbf-installer/trust",
            "/etc",
            "/etc/kbf",
        ] {
            root_entry(&mut exec, dir, true, b"");
        }
        root_entry(&mut exec, "/usr/share/kbf-installer/trust/gh", false, b"");
        root_entry(
            &mut exec,
            "/usr/share/kbf-installer/trust/trusted_root.jsonl",
            false,
            b"",
        );
        let settings = format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", self.me.uid, self.me.gid);
        root_entry(
            &mut exec,
            "/usr/share/kbf-installer/trust/installer.conf",
            false,
            settings.as_bytes(),
        );
        let url = if self.setup.down {
            format!("https://localhost:{}", closed_port())
        } else {
            self.server.url()
        };
        let source = if self.setup.frontend_source {
            String::new()
        } else {
            format!("release_source = \"url\"\nrelease_url = \"{url}\"\n")
        };
        let toml = format!(
            "{source}max_component_bytes = {}\n",
            self.setup.max_component_bytes
        );
        root_entry(&mut exec, "/etc/kbf/installer.toml", false, toml.as_bytes());
        exec
    }

    fn fetcher(&self) -> ReleaseFetcher {
        ReleaseFetcher::new(
            self.layout.clone(),
            HostPath::new("/etc/kbf/installer.toml").expect("path"),
            PathBuf::from(BIN),
            Box::new(self.trust()),
            Box::new(SharedTools(Arc::clone(&self.tools))),
            Box::new(Idle),
        )
        .owned_by(self.me)
        .trusting(self.root.join("test-ca.pem"))
    }

    fn helper(&self, host: ScratchHost) -> Helper<ScratchHost> {
        let cfg = Config {
            update_dir: HostPath::new("/var/lib/kbf-installer").expect("path"),
            orders_dir: HostPath::new("/var/lib/kbf-installer/orders").expect("path"),
            restart: Restart::Systemd("kbf.service".to_owned()),
            window_secs: 600,
            starts: 3,
            min_activation_interval_secs: 0,
        };
        let mut helper = Helper::new(
            self.layout.clone(),
            host,
            cfg,
            Box::new(self.fetcher()),
            Box::new(Idle),
            Box::new(NoProcs),
        );
        helper.recover().expect("recover");
        helper.cycle(T0 - 100).expect("first pass");
        helper
    }

    /// Serve `bytes` as the tarball at `files/<its sha256>`.
    fn serve_tarball(&self, bytes: &[u8]) {
        self.server.set(&format!("/files/{}", sha(bytes)), 200, bytes);
    }

    /// The order `kbf-update` writes for an activation of B: the locator of `tarball` as the
    /// offer carried it.
    fn activate_order(&self, locator: &Locator) {
        let record = OrderRecord {
            order_id: 1,
            epoch: 1,
            generation: 1,
            verb: Verb::Activate,
            commit: B.to_owned(),
            window_s: 600,
            components: vec![locator.clone()],
            expected: Some(expected()),
        };
        fs::write(
            self.root.join(UPDATE).join("orders").join(record.file_name()),
            record.to_bytes().expect("bytes"),
        )
        .expect("record");
    }

    fn locator(tarball: &[u8]) -> Locator {
        Locator {
            name: "kbf".to_owned(),
            sha256: sha(tarball),
            size: tarball.len() as u64,
            attestation: good_bundle(tarball),
        }
    }

    fn status(&self) -> serde_json::Value {
        let bytes = fs::read(self.root.join(UPDATE).join("status.json")).expect("status");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn state(&self) -> State {
        kbf_installer::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    fn release(&self, commit: &str) -> PathBuf {
        self.root.join(UPDATE).join("releases").join(commit)
    }

    /// The names in a directory, sorted.
    fn names(&self, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = match fs::read_dir(dir) {
            Ok(rd) => rd
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// A refusal leaves the host as it was: B is not in `releases/`, nothing of its scratch is
    /// in `work/`, nothing is staged and A still runs.
    fn assert_nothing_left(&self) {
        let releases = self.names(&self.root.join(UPDATE).join("releases"));
        assert!(
            !releases.iter().any(|n| n.starts_with(&B[..8])),
            "releases/ holds {releases:?}"
        );
        let work = self.names(&self.root.join(UPDATE).join("work"));
        assert!(
            !work.iter().any(|n| n.starts_with(B)),
            "work/ holds {work:?}"
        );
        let state = self.state();
        assert_eq!((state.current, state.staged), (Some(c(A)), None));
    }
}

fn locator_for_frontend_update() -> ReleaseLocator {
    ReleaseLocator {
        commit: c(B),
        components: Vec::new(),
        expected: None,
    }
}

// -------------------------------------------------------------------------------- activate

/// An `activate` order for a commit not on the host is pulled from the url source's
/// `files/<sha256>` by the child, passes the pipeline, is installed with its marker, and the
/// helper flips to it on trial. The programs ran in the contract's order: `gh` once on the
/// file the parent holds, then the probe on the unpacked copy in scratch, never on anything
/// under `releases/`.
///
/// Catches a fetcher that is not called, one that installs without the marker or in the wrong
/// directory, a flip before the install, and a probe run from `releases/`.
///
/// Mutants seen red: `Box::new(self.fetcher())` replaced by the refusing `NotWired` (the
/// order is refused); the pipeline's `install` skipped (no directory, no flip).
#[test]
fn an_activate_order_fetches_verifies_installs_and_flips() {
    let (rig, host) = Rig::new(Setup::default());
    let bytes = tarball("release b");
    rig.serve_tarball(&bytes);
    let mut helper = rig.helper(host);
    rig.activate_order(&Rig::locator(&bytes));
    helper.cycle(T0).expect("pass");

    let status = rig.status();
    assert_eq!(status["state"], "activated_unconfirmed", "{status}");
    let state = rig.state();
    assert_eq!(
        (state.current, state.previous, state.staged),
        (Some(c(B)), Some(c(A)), None)
    );
    let link = fs::read_link(rig.root.join(UPDATE).join("releases/current")).expect("link");
    assert_eq!(link.file_name().and_then(|n| n.to_str()), Some(B));
    assert_eq!(
        rig.names(&rig.release(B)),
        ["VERIFIED", "kbf"],
        "the release is its component and the marker"
    );
    assert_eq!(
        fs::read_to_string(rig.release(B).join("VERIFIED")).expect("marker"),
        format!("{B}\n")
    );
    assert_eq!(rig.server.hits(), [format!("/files/{}", sha(&bytes))]);
    assert_eq!(rig.tools.programs("gh").len(), 1);
    let probes: Vec<Spawn> = rig
        .tools
        .calls
        .lock()
        .expect("lock")
        .iter()
        .filter(|s| !s.program.file_name().is_some_and(|n| n == "gh"))
        .cloned()
        .collect();
    assert_eq!(probes.len(), 1);
    assert!(
        probes[0].program.starts_with(rig.root.join(UPDATE).join("work")),
        "the probe ran {}",
        probes[0].program.display()
    );
    assert_eq!(
        rig.names(&rig.root.join(UPDATE).join("work")),
        Vec::<String>::new(),
        "no scratch is left"
    );
}

/// Each refusal of the pipeline keeps its stable word through the cycle, and leaves nothing
/// behind: no directory in `releases/`, no scratch in `work/`, nothing staged, A still current.
///
/// - `hash`: the server sends other bytes of the declared length.
/// - `attestation`: the bundle in the order does not attest the file.
/// - `size`: the order declares more than `max_component_bytes`.
///
/// Mutants seen red: `Fault::into_error` mapping every refusal to `Unavailable` (the words
/// are gone); the digest comparison of `fetch_component` removed (the `hash` case passes the
/// fetch and fails later as another word).
#[test]
fn refusals_keep_their_stable_words_and_leave_nothing_behind() {
    // (name, what the server serves, the locator, the word the refusal begins with)
    let good = tarball("release b");
    let other: Vec<u8> = good.iter().map(|b| b ^ 0x55).collect();
    let cases: Vec<(&str, Vec<u8>, Locator, &str)> = vec![
        ("hash", other, Rig::locator(&good), "rejected: hash: "),
        (
            "attestation",
            good.clone(),
            Locator {
                attestation: "{\"subject\":\"not this file\"}\n".to_owned(),
                ..Rig::locator(&good)
            },
            "rejected: attestation: ",
        ),
        (
            "size",
            good.clone(),
            Locator {
                size: 2 * MIB as u64,
                ..Rig::locator(&good)
            },
            "rejected: size: ",
        ),
    ];
    for (name, served, locator, word) in cases {
        let (rig, host) = Rig::new(Setup::default());
        rig.server.set(&format!("/files/{}", locator.sha256), 200, &served);
        let mut helper = rig.helper(host);
        rig.activate_order(&locator);
        helper.cycle(T0).expect("pass");
        let status = rig.status();
        assert_eq!(status["state"], "refused", "{name}: {status}");
        let why = status["refusal"].as_str().expect("text");
        assert!(
            why.starts_with(&format!("order 1: fetch: {word}")),
            "{name}: {why}"
        );
        rig.assert_nothing_left();
    }
}

/// A source that cannot be reached is `fetch`, not a verdict on the release: the text begins
/// `fetch:` and does not say `rejected`; and a body longer than the order declared is cut off
/// and is `fetch` too. Nothing is left behind.
///
/// Mutant seen red: a source error classed as a rejection (the text then says `rejected`).
#[test]
fn a_source_that_is_down_or_too_long_is_fetch() {
    let good = tarball("release b");
    {
        let (rig, host) = Rig::new(Setup {
            down: true,
            ..Setup::default()
        });
        let mut helper = rig.helper(host);
        rig.activate_order(&Rig::locator(&good));
        helper.cycle(T0).expect("pass");
        let status = rig.status();
        let why = status["refusal"].as_str().expect("text");
        assert_eq!(status["state"], "refused", "{status}");
        assert!(why.starts_with("order 1: fetch: "), "{why}");
        assert!(!why.contains("rejected"), "{why}");
        rig.assert_nothing_left();
    }
    {
        let (rig, host) = Rig::new(Setup::default());
        let mut long = good.clone();
        long.extend_from_slice(&[7; 100]);
        let locator = Rig::locator(&good);
        rig.server.set(&format!("/files/{}", locator.sha256), 200, &long);
        let mut helper = rig.helper(host);
        rig.activate_order(&locator);
        helper.cycle(T0).expect("pass");
        let why = rig.status()["refusal"].as_str().expect("text").to_owned();
        assert!(why.starts_with("order 1: fetch: "), "{why}");
        assert!(why.contains("more than"), "{why}");
        rig.assert_nothing_left();
    }
}

// ------------------------------------------------------------------------- frontend_update

/// The index of commit B on the server: `kbf`'s digest and size, and its bundle.
fn serve_index(rig: &Rig, bytes: &[u8], bundle: &str) {
    let base = format!("/index/{B}/{}/kbf", platform());
    rig.server
        .set(&base, 200, format!("{} {}\n", sha(bytes), bytes.len()).as_bytes());
    rig.server
        .set(&format!("{base}.attestation"), 200, bundle.as_bytes());
}

fn report(got: Result<InstalledRelease, FetchError>) -> InstalledRelease {
    got.unwrap_or_else(|e| panic!("not fetched: {e}"))
}

/// A `frontend_update` locator carries no components: the fetcher reads the commit's index
/// entry and bundle from the source, pulls the tarball it names, verifies and installs it, and
/// records it as staged. No link moves and `current` stays A. The report names the commit and
/// the platform of the binary's header, which the cycle checks.
///
/// Catches a locator with no components treated as an empty activation (the pipeline panics or
/// refuses), a frontend update installed as an activation (it needs facts it has not), the
/// index read from a path other than the store's, and a staged release not recorded.
///
/// Mutants seen red: the kind chosen as `Activate` for an empty component list (the pipeline
/// refuses for missing facts); `staged` not written by the install.
#[test]
fn a_frontend_update_resolves_the_index_and_stages_without_a_flip() {
    let (rig, _host) = Rig::new(Setup::default());
    let bytes = tarball("release b");
    serve_index(&rig, &bytes, &good_bundle(&bytes));
    rig.serve_tarball(&bytes);

    let got = report(rig.fetcher().fetch(&locator_for_frontend_update()));
    assert_eq!(got.commit, c(B));
    assert_eq!(got.platform, platform());
    assert_eq!(got.profile, "release");

    let state = rig.state();
    assert_eq!((state.current, state.staged), (Some(c(A)), Some(c(B))));
    assert_eq!(rig.names(&rig.release(B)), ["VERIFIED", "kbf"]);
    assert!(
        !rig.root.join(UPDATE).join("releases/current").exists()
            || fs::read_link(rig.root.join(UPDATE).join("releases/current"))
                .is_ok_and(|l| l.file_name().is_some_and(|n| n != B))
    );
    let base = format!("/index/{B}/{}/kbf", platform());
    assert_eq!(
        rig.server.hits(),
        [
            base.clone(),
            format!("{base}.attestation"),
            format!("/files/{}", sha(&bytes))
        ]
    );
    assert_eq!(rig.tools.programs("gh").len(), 1);
}

/// What can go wrong with the index of a frontend update, each with its word and nothing left:
/// no entry (`fetch`), an entry whose digest is not the tarball's (`hash`), a bundle that does
/// not attest the tarball (`attestation`), an entry that is empty, too long or not a digest and
/// a size (`fetch`), and a frontend source, which has no index (`fetch`).
///
/// Mutants seen red: the entry's line accepted without its line end or with a size of `+5`
/// (the malformed cases pass the parse); `receive_up_to` demanding the full limit (the empty
/// and short cases are `Short`, but so is every honest entry, so the happy path goes red).
#[test]
fn a_frontend_update_refuses_with_the_word_of_what_failed() {
    let bytes = tarball("release b");
    let word = |rig: &Rig| match rig.fetcher().fetch(&locator_for_frontend_update()) {
        Ok(_) => "installed".to_owned(),
        Err(FetchError::Rejected(why)) => format!("rejected: {why}"),
        Err(other) => other.to_string(),
    };
    let base = format!("/index/{B}/{}/kbf", platform());

    // No entry at all.
    let (rig, _host) = Rig::new(Setup::default());
    assert!(word(&rig).starts_with("fetch: "), "{}", word(&rig));
    rig.assert_nothing_left();

    // The entry names a digest the tarball does not have.
    let (rig, _host) = Rig::new(Setup::default());
    let claimed = sha(b"something else");
    rig.server.set(&base, 200, format!("{claimed} {}\n", bytes.len()).as_bytes());
    rig.server
        .set(&format!("{base}.attestation"), 200, good_bundle(&bytes).as_bytes());
    rig.server.set(&format!("/files/{claimed}"), 200, &bytes);
    assert!(word(&rig).starts_with("rejected: hash: "), "{}", word(&rig));
    rig.assert_nothing_left();

    // A bundle for other bytes.
    let (rig, _host) = Rig::new(Setup::default());
    serve_index(&rig, &bytes, &good_bundle(b"other"));
    rig.serve_tarball(&bytes);
    assert!(word(&rig).starts_with("rejected: attestation: "), "{}", word(&rig));
    rig.assert_nothing_left();

    // An entry that is not `<sha256> <size>\n`.
    for line in [
        String::new(),
        format!("{} 5", sha(&bytes)),
        format!("{} +5\n", sha(&bytes)),
        format!("{} 5 6\n", sha(&bytes)),
        "nothing\n".to_owned(),
        format!("{}\n", "x".repeat(300)),
    ] {
        let (rig, _host) = Rig::new(Setup::default());
        rig.server.set(&base, 200, line.as_bytes());
        let why = word(&rig);
        assert!(why.starts_with("fetch: "), "{line:?}: {why}");
        rig.assert_nothing_left();
    }

    // The frontend source has no index.
    let (rig, _host) = Rig::new(Setup {
        frontend_source: true,
        ..Setup::default()
    });
    let why = word(&rig);
    assert!(why.starts_with("fetch: ") && why.contains("not wired"), "{why}");
    assert!(rig.server.hits().is_empty());
    rig.assert_nothing_left();
}

/// A `work/<commit>.partial/` left by a pass that died does not hold the commit's fetches off
/// for good: the next fetch removes it and goes on.
///
/// Mutant seen red: the sweep of `work/<commit>.partial` removed (the stage refuses a
/// directory it did not make, and every later fetch of the commit with it).
#[test]
fn a_partial_directory_of_a_dead_pass_is_swept() {
    let (rig, _host) = Rig::new(Setup::default());
    let stale = rig.root.join(UPDATE).join("work").join(format!("{B}.partial"));
    fs::create_dir_all(stale.join("dl")).expect("stale");
    fs::write(stale.join("dl/kbf.tar.gz"), b"half").expect("half");
    let bytes = tarball("release b");
    serve_index(&rig, &bytes, &good_bundle(&bytes));
    rig.serve_tarball(&bytes);
    report(rig.fetcher().fetch(&locator_for_frontend_update()));
    assert_eq!(rig.state().staged, Some(c(B)));
}

/// A locator that names components and no facts, or facts and no components, is neither an
/// activation nor a frontend update and is refused before the source is contacted.
///
/// Mutant seen red: the match on `(components.is_empty(), expected)` collapsed to the
/// components alone (a locator with components and no facts reaches the pipeline).
#[test]
fn a_locator_that_is_neither_kind_is_refused_before_the_network() {
    let (rig, _host) = Rig::new(Setup::default());
    let bytes = tarball("release b");
    let mut both = locator_for_frontend_update();
    both.expected = Some(expected());
    let mut none = locator_for_frontend_update();
    none.components = vec![Rig::locator(&bytes)];
    for rec in [both, none] {
        assert!(matches!(
            rig.fetcher().fetch(&rec),
            Err(FetchError::Unavailable(_))
        ));
    }
    assert!(rig.server.hits().is_empty());
}

// --------------------------------------------------------------------------------- the child

fn child(args: &[&str]) -> (bool, String) {
    let out = Command::new(BIN)
        .arg("fetch-child")
        .args(args)
        .env_clear()
        .output()
        .expect("runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The child checks the flags of an index read again, whatever the parent checked: a blob and
/// an entry together or neither, an entry that is a path or the wrong shape, an entry from the
/// frontend source.
///
/// Mutants seen red: the entry's shape check dropped (`../x` reaches the URL); the
/// exactly-one rule dropped (both flags fetch the blob).
#[test]
fn the_child_checks_the_flags_of_an_index_read() {
    let digest = sha(b"x");
    let entry = format!("{B}/{}/kbf", platform());
    let base = ["--kind", "url", "--url", "https://h.example", "--limit", "10"];
    let with = |extra: &[&str]| {
        let mut args: Vec<&str> = base.to_vec();
        args.extend_from_slice(extra);
        child(&args)
    };
    let (ok, err) = with(&["--index", &entry, "--sha256", &digest]);
    assert!(!ok && err.contains("exactly one"), "{err}");
    let (ok, err) = with(&[]);
    assert!(!ok && err.contains("exactly one"), "{err}");
    for bad in [
        "../x",
        &format!("{B}/{}/kbf/extra", platform()),
        &format!("{B}/linux-x86_64"),
        &format!("{B}/plan9/kbf"),
        &format!("{}/{}/kbf", &B[..39], platform()),
        &format!("{B}/{}/Kbf", platform()),
        &format!("{B}/{}/kbf.other", platform()),
    ] {
        let (ok, err) = with(&["--index", bad]);
        assert!(!ok && err.contains("not an index entry"), "{bad}: {err}");
    }
    let (ok, err) = child(&[
        "--kind",
        "frontend",
        "--url",
        "https://h.example",
        "--limit",
        "10",
        "--index",
        &entry,
    ]);
    assert!(!ok && err.contains("url source"), "{err}");
}
