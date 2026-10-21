//! The frontend source of the host fetcher (`frontend_source`): that it is built from the
//! snapshot, the node's pair and a CA copy that passed every guard, that each guard refuses
//! with its own reason, and that a host that passes them all is still refused until the fetch
//! is enabled. The rest of the host fetcher is in `fetcher_host_tests.rs`.

use std::io;
use std::process::Output;

use kbf_host::exec::Meta;
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::trust::Untrusted;

use super::*;
use crate::fetch::identity::MAX_PEM_BYTES;

const CERT_PATH: &str = "/etc/kbf/pki/node.crt";
const KEY_PATH: &str = "/etc/kbf/pki/node.key";
const URL_LINE: &str = "frontend_url = \"https://frontend.lan:8443\"\n";

/// A CA as far as the guards go: they look at its owner, mode, size and digest, not at its PEM.
const CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nZnJvbnRlbmQ=\n-----END CERTIFICATE-----\n";

/// A host that remembers which paths it was asked to read and to stat, and can fail a read
/// (a stat is the fake's own).
struct Watched {
    inner: FakeExec,
    read: RefCell<Vec<String>>,
    statted: RefCell<Vec<String>>,
    fail_read: bool,
}

impl Watched {
    /// The host `disk` says, with nothing asked of it yet.
    fn of(disk: &Disk) -> Watched {
        Watched {
            inner: exec(disk),
            read: RefCell::new(Vec::new()),
            statted: RefCell::new(Vec::new()),
            fail_read: false,
        }
    }

    /// The paths of the node's identity that were read or stat-ed.
    fn node_files(&self) -> Vec<String> {
        let mut all = self.read.borrow().clone();
        all.extend(self.statted.borrow().iter().cloned());
        all.retain(|p| p.starts_with("/etc/kbf/pki/"));
        all
    }
}

impl Exec for Watched {
    fn euid(&self) -> u32 {
        self.inner.euid()
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.read.borrow_mut().push(path.to_string());
        if self.fail_read {
            return Err(io::Error::other("planted: the read failed"));
        }
        self.inner.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.statted.borrow_mut().push(path.to_string());
        self.inner.stat(path)
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.inner.query(cmd)
    }

    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.inner.http_status(url)
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        self.inner.apply(step)
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        self.inner.check_binary()
    }
}

/// `bytes` as a CA copy that root owns, mode 0644.
fn ca_copy(bytes: &[u8]) -> CaCopy {
    CaCopy {
        bytes: bytes.to_vec(),
        uid: 0,
        mode: 0o644,
    }
}

/// The test CA, owned by `uid` with mode `mode`.
fn ca_with(uid: u32, mode: u32) -> CaCopy {
    CaCopy {
        bytes: CA_PEM.to_vec(),
        uid,
        mode,
    }
}

/// The host with the CA copy `ca` (`None` for none), root's directories and nothing else.
fn host_with(ca: Option<CaCopy>) -> Watched {
    Watched::of(&Disk {
        ca,
        ..Disk::default()
    })
}

/// A directory with mode 0755 that `uid` owns.
fn dir_of(uid: u32) -> Entry {
    Entry::Dir {
        perms: Perms::new(0o755),
        owner: Owner { uid, gid: 0 },
    }
}

/// A link to `target`, which is never followed.
fn link_to(target: &str) -> Entry {
    Entry::Link {
        target: PathBuf::from(target),
    }
}

/// Puts `entry` at `at` on the fake host under `host`.
fn put(host: &mut Watched, at: &str, entry: Entry) {
    host.inner.entries.insert(path(at), entry);
}

/// The sha256 of `bytes`, as `frontend_ca_sha256` holds it.
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The node's certificate and key, as the lines of `installer.toml`.
fn node_lines() -> String {
    format!("node_cert = \"{CERT_PATH}\"\nnode_key = \"{KEY_PATH}\"\n")
}

/// The `installer.toml` of a LAN-pull host whose CA copy has the sha256 `ca_digest`.
fn pull_file(ca_digest: &str) -> String {
    let nodes = node_lines();
    let ca = format!(
        "frontend_ca = \"{CA_PATH}\"\nfrontend_ca_sha256 = \"{ca_digest}\"\n"
    );
    format!("release_source = \"frontend\"\n{nodes}{URL_LINE}{ca}")
}

fn parsed(text: &str) -> InstallerToml {
    InstallerToml::parse(text).expect("parses")
}

/// Why `frontend_source` refuses `toml` on `host`.
fn refused_toml(toml: &InstallerToml, host: &Watched) -> String {
    let got = frontend_source(toml, host);
    got.expect_err("refused").to_string()
}

/// Why `frontend_source` refuses the file `text` on `host`.
fn refused(text: &str, host: &Watched) -> String {
    refused_toml(&parsed(text), host)
}

/// `why` says `part`.
fn says(why: &str, part: &str) {
    assert!(why.contains(part), "{part:?} is not in {why:?}");
}

/// `frontend_source` refuses the file `text` on `host`, and says `part`.
fn refusal(text: &str, host: &Watched, part: &str) {
    says(&refused(text, host), part);
}

/// The frontend source is built from what `installer.toml` names: the worker listener's
/// address, the CA copy the snapshot names (and not a CA beside the node's identity), and the
/// node's certificate and key, each in its own field. A CA copy of one byte and one of the most
/// the identity reader takes are both accepted. The CA is read once, and no file of the node's
/// identity is read or stat-ed while the source is built.
///
/// Catches a field taken from the wrong key (the address and the CA swapped, the CA taken from
/// the node's `pki` directory, the certificate and key swapped), a field dropped, a size bound
/// off by one at either end, and a node file read when the source is built.
///
/// Mutants to plant: `url` taken from `frontend_ca`; `ca_file` taken from the node's
/// `pki/ca.pem`; `node_cert` and `node_key` swapped; the size range `2..=cap` or `1..cap`.
#[test]
fn the_frontend_source_is_built_from_the_snapshot_the_node_pair_and_a_pinned_ca() {
    let (one, most) = (vec![b'a'], vec![b'a'; MAX_PEM_BYTES as usize]);
    for bytes in [one, CA_PEM.to_vec(), most] {
        let text = pull_file(&digest(&bytes));
        let host = host_with(Some(ca_copy(&bytes)));
        let built = frontend_source(&parsed(&text), &host).expect("built");
        let want = FrontendSource {
            url: "https://frontend.lan:8443".to_owned(),
            ca_file: PathBuf::from(CA_PATH),
            node_cert: PathBuf::from(CERT_PATH),
            node_key: PathBuf::from(KEY_PATH),
        };
        assert_eq!(built, want, "{} bytes", bytes.len());
        assert_eq!(*host.read.borrow(), [CA_PATH]);
        let nodes = host.node_files();
        assert!(nodes.is_empty(), "{nodes:?}");
    }
}

/// The watching host passes every call it does not record on to the host it wraps, so the
/// tests above see the host they set up: its user, a command and its answer, a URL's status, its
/// binary check, and a step applied to its files.
///
/// Catches a delegate that answers for itself (a user other than the wrapped host's), drops a
/// query, or does not apply a step.
///
/// Mutants to plant: `apply` returning `Ok(())` without forwarding (red: the CA is still
/// there); `query` answering without recording it on the wrapped host.
#[test]
fn the_watching_host_passes_its_other_calls_on_to_the_host_it_wraps() {
    let mut host = host_with(Some(ca_copy(CA_PEM)));
    assert_eq!(host.euid(), 0);
    let cmd = Cmd::new(path("/bin/true"));
    let answer = host.query(&cmd).expect("answers");
    assert!(answer.status.success());
    assert_eq!(host.inner.queries.borrow().len(), 1);
    let url = HttpUrl::new("http://127.0.0.1:1/").expect("a url");
    assert_eq!(host.http_status(&url).expect("answers"), 200);
    host.check_binary().expect("trusted");
    let ca = path(CA_PATH);
    host.apply(&Step::Remove { path: ca }).expect("applies");
    assert!(!host.inner.entries.contains_key(&path(CA_PATH)));
}

/// A frontend host that passes every guard is refused all the same, with the reason that this
/// build does not fetch from the frontend yet, from `source_of` and through the host fetcher
/// (`assemble`, and `fetch`, which says `Unavailable`), and no node key is read or lent: the
/// source that was built is dropped. The same host without its CA copy is refused for that.
///
/// Catches the built source handed to the fetch (the node key lent to the child before the
/// decision on it), a node file read while the guards run, and the guards not reached from
/// the host fetcher.
///
/// Mutants to plant: the arm returning `Source::Frontend` (red: no refusal); the arm reading
/// `node_key` (red: the node files asked for); `assemble` handing `source_of` an executor other
/// than `parts.trust` (red: the Rig case sees no CA, so its message differs).
#[test]
fn a_frontend_host_that_passes_every_guard_is_still_refused_and_reads_no_node_key() {
    let text = pull_file(&digest(CA_PEM));
    let host = host_with(Some(ca_copy(CA_PEM)));
    let got = source_of(&parsed(&text), &host);
    let why = got.expect_err("refused").to_string();
    says(&why, "does not fetch from the frontend yet");
    assert_eq!(*host.read.borrow(), [CA_PATH]);
    let nodes = host.node_files();
    assert!(nodes.is_empty(), "{nodes:?}");

    let rig = Rig::new("host-fetcher-frontend-guarded");
    rig.write(&text);
    rig.disk.borrow_mut().ca = Some(ca_copy(CA_PEM));
    assert_eq!(rig.refused(), why);
    let want = FetchError::Unavailable(why);
    assert_eq!(rig.fetcher.fetch(&locator()), Err(want));
    rig.disk.borrow_mut().ca = None;
    says(&rig.refused(), "frontend-ca.pem is not trusted");
}

/// Each piece the frontend source needs is refused with its own reason, before the next is
/// looked at, and the reason of a digest that does not match holds neither digest nor any of
/// the file's bytes:
///
/// - the snapshot is not in the file (nothing, the source alone, the node pair alone);
/// - the node's pair is not named (the snapshot alone, and either half);
/// - the CA path is not one a host path can be;
/// - the CA copy is missing, owned by another user, writable by group or others, behind a
///   directory another user owns, a link, or cannot be statted;
/// - it is empty, or one byte over what the identity reader takes (with the digest of those
///   very bytes, so only the size refuses);
/// - it cannot be read;
/// - its bytes are not the ones `frontend_ca_sha256` names, or the digest names other bytes.
///
/// Catches each guard missing, inverted, reduced or compared against the wrong value: the
/// ownership check on the file only and not the directories above it, the link followed, the
/// size read after the digest, the digest compared with a different key, a missing file taken
/// for an empty one, and a reason that is the next guard's.
///
/// Mutants to plant: the digest check removed; `trusted_chain` replaced by a check of the file
/// alone; the digest compared with `frontend_url`; the size check removed; the node pair
/// accepted when absent or half there; the size check ahead of the chain (the link case reads
/// "empty").
#[test]
fn the_frontend_source_is_refused_with_its_reason_for_each_missing_or_untrusted_piece() {
    let digest_of_pem = digest(CA_PEM);
    let good = pull_file(&digest_of_pem);
    let host = host_with(Some(ca_copy(CA_PEM)));

    let nodes = node_lines();
    let pair_only = format!("release_source = \"frontend\"\n{nodes}");
    let source_only = "release_source = \"frontend\"\n";
    for text in ["", source_only, pair_only.as_str()] {
        refusal(text, &host, "needs frontend_url");
    }

    let no_pair = format!("release_source = \"frontend\"\n{SNAPSHOT}");
    refusal(&no_pair, &host, "needs node_cert and node_key");
    let mut toml = parsed(&good);
    toml.node_key = None;
    let why = refused_toml(&toml, &host);
    says(&why, "needs node_cert and node_key");
    toml.node_key = Some(PathBuf::from(KEY_PATH));
    toml.node_cert = None;
    let why = refused_toml(&toml, &host);
    says(&why, "needs node_cert and node_key");

    let mut toml = parsed(&good);
    toml.frontend.as_mut().expect("a snapshot").ca = PathBuf::from("relative/ca.pem");
    says(&refused_toml(&toml, &host), "is not absolute");

    let gone = host_with(None);
    let why = refused(&good, &gone);
    says(&why, "frontend-ca.pem is not trusted");
    says(&why, "it is missing");

    let theirs = host_with(Some(ca_with(1000, 0o644)));
    refusal(&good, &theirs, "owned by uid 1000, not root");
    for mode in [0o666, 0o664, 0o646] {
        let loose = host_with(Some(ca_with(0, mode)));
        refusal(&good, &loose, "writable by group or others");
    }

    let mut behind = host_with(Some(ca_copy(CA_PEM)));
    put(&mut behind, "/var/lib/kbf-installer", dir_of(1000));
    let why = refused(&good, &behind);
    says(&why, "/var/lib/kbf-installer is not trusted");
    says(&why, "owned by uid 1000, not root");

    let mut linked = host_with(Some(ca_copy(CA_PEM)));
    put(&mut linked, CA_PATH, link_to("/etc/other.pem"));
    refusal(&good, &linked, "symbolic link");

    let unreadable = good.replace(CA_PATH, "/unreadable");
    refusal(&unreadable, &host, "it cannot be read");

    let part = format!("empty or over {MAX_PEM_BYTES} bytes");
    let most_and_one = vec![b'a'; MAX_PEM_BYTES as usize + 1];
    for bytes in [Vec::new(), most_and_one] {
        let sized = host_with(Some(ca_copy(&bytes)));
        refusal(&pull_file(&digest(&bytes)), &sized, &part);
    }

    let mut failing = host_with(Some(ca_copy(CA_PEM)));
    failing.fail_read = true;
    refusal(&good, &failing, "could not be read");

    let mut flipped = CA_PEM.to_vec();
    flipped[0] ^= 1;
    let altered = host_with(Some(ca_copy(&flipped)));
    let why = refused(&good, &altered);
    says(&why, "does not have the sha256");
    assert!(!why.contains(&digest_of_pem), "{why}");
    assert!(!why.contains("BEGIN CERTIFICATE"), "{why}");
    let other = pull_file(&digest(&flipped));
    refusal(&other, &host, "does not have the sha256");
}
