//! The real fetcher (`kbf_installer::helper::fetcher_real::RealFetcher`) end to end on a scratch
//! host: the fetch child is the stand-in of `tests/stub/fetch_stub.rs` (it serves a file the
//! test names, or dies as told), started by `fetch_component` exactly as on a host; the verify
//! pipeline is the real one, with `gh` and the binary's `--version --json` played by a fake
//! runner. What each test proves is named on it, with the mutant that turns it red.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::step::Owner;
use kbf_installer::CommitId;
use kbf_installer::binary::Arch;
use kbf_installer::fetch::{ChildProgram, Limits, Source};
use kbf_installer::fixtures::{
    COMMIT, FakeRunner, elf, file, me, ran, tar_gz, trusted_host_on, version_json,
};
use kbf_installer::helper::fetcher::{
    FetchError, Fetcher, InstalledRelease, NotWired, ReleaseLocator,
};
use kbf_installer::helper::fetcher_real::{NO_COMPONENT_LIST, RealFetcher};
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::layout::Layout;
use kbf_installer::settings::Settings;
use kbf_installer::stage::{Policy, Supported, Timeouts};
use kbf_installer::testkit::host_root;
use kbf_update::{Facts, Locator};
use sha2::{Digest, Sha256};

const STUB: &str = env!("CARGO_BIN_EXE_kbf-fetch-stub");

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn commit() -> CommitId {
    CommitId::new(COMMIT).expect("commit")
}

/// The facts `version_json` makes a binary declare.
fn facts() -> Facts {
    Facts {
        platform: "linux-x86_64".to_owned(),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: "release".to_owned(),
    }
}

/// What the installed release is reported as.
fn installed() -> InstalledRelease {
    InstalledRelease {
        commit: commit(),
        platform: "linux-x86_64".to_owned(),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: "release".to_owned(),
    }
}

/// The tarball of an honest x86-64 Linux `kbf`.
fn tarball() -> Vec<u8> {
    tar_gz(&[file("kbf", &elf(0x3e, "payload"))])
}

/// The locator an order carries for `bytes`, as the component called `name`.
fn locator(name: &str, bytes: &[u8]) -> Locator {
    Locator {
        name: name.to_owned(),
        sha256: sha(bytes),
        size: bytes.len() as u64,
        attestation: "{\"mediaType\":\"bundle\"}\n".to_owned(),
    }
}

fn rec(components: Vec<Locator>) -> ReleaseLocator {
    ReleaseLocator {
        commit: commit(),
        components,
    }
}

/// A host on which no lease is live.
struct NoLeases;

impl LeaseSource for NoLeases {
    fn live(&self) -> Result<usize, String> {
        Ok(0)
    }
}

/// How the programs the pipeline runs answer.
struct Answers {
    /// `gh attestation verify`: (exit code, stderr).
    gh: (i32, &'static str),
    /// `<binary> --version --json`: its stdout.
    probe: String,
}

impl Answers {
    fn honest() -> Answers {
        Answers {
            gh: (0, ""),
            probe: version_json(COMMIT, "release"),
        }
    }

    fn runner(self) -> FakeRunner {
        FakeRunner::new(move |spawn| {
            if spawn.program.file_name().is_some_and(|n| n == "gh") {
                Ok(ran(self.gh.0, "", self.gh.1))
            } else {
                Ok(ran(0, &self.probe, ""))
            }
        })
    }
}

/// A scratch host, and the files the stand-in child serves.
struct Rig {
    host: Layout,
    dir: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let (host, dir) = host_root(&format!("fetcher-real-{name}"));
        fs::create_dir_all(dir.join("served")).expect("served");
        Rig { host, dir }
    }

    /// A file the child can serve.
    fn serve(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join("served").join(name);
        fs::write(&path, bytes).expect("served file");
        path
    }

    /// The child's operations, one per line.
    fn ops(&self, lines: &[String]) -> PathBuf {
        let path = self.dir.join("served").join("ops");
        fs::write(&path, lines.join("\n")).expect("operations");
        path
    }

    /// A fetcher whose child follows `ops` (the stand-in) and whose programs answer as told.
    fn fetcher(&self, ops: Option<&Path>, answers: Answers) -> RealFetcher {
        let ids = me();
        let mut settings = Settings::defaults(&self.host);
        settings.run_as = ids;
        settings.arch = Arch::X86_64;
        RealFetcher {
            host: self.host.clone(),
            settings,
            policy: Policy {
                max_component_bytes: 1 << 20,
                max_binary_bytes: 1 << 20,
                min_protocol: 0,
                supported: Supported {
                    protocol: 10,
                    min_protocol: 10,
                    state_schema: 10,
                },
                timeouts: Timeouts::DEFAULT,
            },
            trust: Box::new(trusted_host_on(self.host.platform())),
            runner: Box::new(answers.runner()),
            leases: Box::new(NoLeases),
            owner: Owner {
                uid: ids.uid,
                gid: ids.gid,
            },
            child: ChildProgram {
                program: match ops {
                    Some(_) => PathBuf::from(STUB),
                    None => PathBuf::from("/nonexistent/kbf-installer"),
                },
                run_as: None,
            },
            source: Source::Url {
                base: "https://stub.example/kbf".to_owned(),
                ca_file: ops.map(Path::to_path_buf),
            },
            limits: Limits {
                max_component_bytes: 1 << 20,
                fetch_timeout: Duration::from_secs(60),
            },
        }
    }

    fn work(&self) -> PathBuf {
        self.host.join(&self.host.work_dir())
    }

    /// What is left in the scratch directory (`work/`); nothing when it was never made.
    fn work_left(&self) -> Vec<String> {
        match fs::read_dir(self.work()) {
            Ok(entries) => entries
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn release(&self) -> PathBuf {
        self.host.join(&self.host.release_dir(&commit()))
    }

    /// The child serves `bytes` whole.
    fn serving(&self, bytes: &[u8]) -> PathBuf {
        let path = self.serve("blob", bytes);
        self.ops(&[format!("copy:{}", path.display())])
    }
}

/// An activation of the honest release: the child brings the tarball, the pipeline checks it
/// (the attestation, the shape, the binary's own report against the order's facts) and
/// installs it, and the fetcher reports what the binary declared.
///
/// Catches the joint broken anywhere: a fetch that does not reach the pipeline, the pipeline
/// given no expected facts (an activation without them is refused), the files handed over
/// under the wrong name, an installed release without its marker, or scratch left behind.
///
/// Mutants: `expected` passed as `None` (this test goes red: the facts are required);
/// the `Fetched` handed over with no files (red: `kbf was not fetched`).
#[test]
fn an_activation_is_fetched_verified_and_installed() {
    let rig = Rig::new("good");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let got = fetcher
        .fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts())
        .expect("installed");
    assert_eq!(got, installed());
    assert!(rig.release().join("kbf").is_file(), "the binary");
    assert!(rig.release().join("VERIFIED").is_file(), "the marker");
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
}

/// A source that serves the right number of the wrong bytes: the digest the parent computed
/// is not the order's, so the fetch is refused with the contract's `hash` word, and what was
/// made for it is gone.
///
/// Catches the digest not compared (the pipeline would refuse later, and still leave the
/// directory if cleanup failed) and the scratch directory kept after a refusal.
///
/// Mutants: `fetch_component`'s digest comparison removed (red: the refusal then comes from
/// the pipeline, with its own wording); the partial directory forgotten on a failed pull
/// (`std::mem::forget`; red: scratch left).
#[test]
fn a_download_with_the_wrong_digest_is_refused_and_leaves_nothing() {
    let rig = Rig::new("digest");
    let bytes = tarball();
    let mut wrong = bytes.clone();
    *wrong.last_mut().expect("bytes") ^= 0xff;
    let ops = rig.serving(&wrong);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Rejected(why)) => {
            assert!(why.starts_with("hash: the download has sha256 "), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A child killed after some of the bytes: the stream is short, which is an outage (`fetch`,
/// not a verdict), and the scratch directory is removed.
///
/// Catches a short read taken for a download, and the directory kept when the pull fails after
/// the file was made.
///
/// Mutant: the partial directory forgotten on a failed pull (red: scratch left).
#[test]
fn a_child_killed_mid_stream_is_an_outage_and_leaves_nothing() {
    let rig = Rig::new("killed");
    let bytes = tarball();
    let blob = rig.serve("blob", &bytes);
    let ops = rig.ops(&[format!("head:20:{}", blob.display()), "kill".to_owned()]);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Unavailable(why)) => {
            assert!(why.starts_with("the child ended after 20 of the "), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A child that cannot be started is an outage too, and the file made for the download goes
/// with the directory.
///
/// Catches a failure before the first byte leaving `work/<commit>.partial/` behind (the next
/// attempt would then be refused as a crashed stage's).
#[test]
fn a_child_that_cannot_start_leaves_nothing() {
    let rig = Rig::new("nospawn");
    let bytes = tarball();
    let fetcher = rig.fetcher(None, Answers::honest());
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Unavailable(why)) => {
            assert!(why.starts_with("cannot start the fetch child"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
}

/// The verify pipeline is what installs: bytes of the right digest that the attestation check
/// does not accept are refused with the contract's `attestation` word, nothing is installed
/// and the scratch directory is gone.
///
/// Catches a fetcher that installs what the child brought without the pipeline's checks.
///
/// Mutant: `Pipeline::verify` skipped (the partial renamed into `releases/` and the order's
/// own facts reported as installed): this test and the one below go red.
#[test]
fn a_release_the_attestation_does_not_accept_is_refused_and_not_installed() {
    let rig = Rig::new("attest");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let answers = Answers {
        gh: (1, "no matching attestation"),
        ..Answers::honest()
    };
    let fetcher = rig.fetcher(Some(&ops), answers);
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Rejected(why)) => assert!(why.starts_with("attestation: "), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A binary that declares other facts than the order expected is refused with `facts`, and
/// not installed: the order's facts reach the pipeline.
///
/// Catches the expected facts dropped on the way (an activation would install whatever the
/// binary says).
///
/// Mutant: the facts handed to the pipeline replaced by those the probe printed (red).
#[test]
fn a_binary_that_declares_other_facts_is_refused() {
    let rig = Rig::new("facts");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let mut expected = facts();
    expected.protocol = 5;
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &expected);
    match got {
        Err(FetchError::Rejected(why)) => assert!(why.starts_with("facts: "), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A `frontend_update` order names no component and nothing yet says where to get them: the
/// fetch says so, makes nothing (no scratch directory, no child) and does not panic.
///
/// Catches a frontend update with no components going on into the pipeline, which has no
/// release to name.
///
/// Mutant: the empty-components check removed (red: the pipeline panics on the empty set).
#[test]
fn a_frontend_update_without_components_says_so_and_makes_nothing() {
    let rig = Rig::new("update");
    let fetcher = rig.fetcher(None, Answers::honest());
    assert_eq!(
        fetcher.fetch(&rec(Vec::new())),
        Err(FetchError::Unavailable(NO_COMPONENT_LIST.to_owned()))
    );
    assert!(!rig.work().exists(), "not even the work directory");
}

/// An activation that names no component is refused the same way (the cycle never sends one,
/// but the fetcher does not rely on that).
///
/// Mutant: the empty-components check removed (red: a panic in the pipeline).
#[test]
fn an_activation_without_components_is_refused_and_makes_nothing() {
    let rig = Rig::new("empty-activation");
    let fetcher = rig.fetcher(None, Answers::honest());
    let got = fetcher.fetch_activation(&rec(Vec::new()), &facts());
    assert!(matches!(got, Err(FetchError::Rejected(_))), "{got:?}");
    assert!(!rig.work().exists(), "not even the work directory");
}

/// A frontend update that does carry its components is fetched and verified without facts to
/// compare with: only the commit and the profile are checked, so a binary that declares
/// protocol 4 is installed where an activation expecting protocol 5 refused it.
///
/// Catches the two entry points sharing one kind (an activation with no expectation, or a
/// frontend update held to facts it was never given).
///
/// Mutant: `fetch` verifying as `OrderKind::Activate` (red: the facts are then required).
#[test]
fn a_frontend_update_with_components_is_verified_without_facts() {
    let rig = Rig::new("update-components");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let got = fetcher
        .fetch(&rec(vec![locator("kbf", &bytes)]))
        .expect("installed");
    assert_eq!(got, installed());
    assert!(rig.release().join("VERIFIED").is_file());
}

/// A component whose name is a path is refused before any file is made: it would have been
/// joined onto root's download directory.
///
/// Catches a name with a separator reaching `dl/<name>.tar.gz` (the file would land outside
/// the directory).
///
/// Mutant: the plain-name check removed (red: the file is made outside, and the stand-in
/// child is started).
#[test]
fn a_component_named_by_a_path_is_refused_before_a_file_is_made() {
    let rig = Rig::new("path-name");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let got = fetcher.fetch_activation(&rec(vec![locator("../evil", &bytes)]), &facts());
    match got {
        Err(FetchError::Rejected(why)) => assert!(why.contains("not a plain file name"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.dir.join("evil.tar.gz").exists());
    assert!(!rig.work().join("evil.tar.gz").exists());
}

/// The pipeline asks the lease source again, after the download: a source that cannot tell
/// (the binary's `leases::NotWired`) refuses with `lease`, nothing is installed and the
/// directory is gone. This is why the binary does not build a `RealFetcher` while it has no
/// real lease source: every activation would be downloaded and then refused.
///
/// Catches the lease step skipped on the real path (a host with work running would be swapped).
///
/// Mutant: `leases` handed to the pipeline as an always-zero source (red).
#[test]
fn a_lease_source_that_cannot_tell_refuses_after_the_download() {
    let rig = Rig::new("leases");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let mut fetcher = rig.fetcher(Some(&ops), Answers::honest());
    fetcher.leases = Box::new(kbf_installer::helper::leases::NotWired);
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Rejected(why)) => assert!(why.starts_with("lease: "), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A scratch directory left by a crashed stage is refused, not reused and not removed: it is
/// not this fetch's to delete (`run` sweeps `work/` at its start).
///
/// Catches a fetch that goes on into a directory it did not make, or removes one it did not.
///
/// Mutant: the fetcher removing `work/<commit>.partial` before it makes its own (red: the
/// directory is gone, and the fetch goes on).
#[test]
fn a_directory_a_crashed_stage_left_is_refused_and_kept() {
    let rig = Rig::new("stale");
    let stale = rig.work().join(format!("{COMMIT}.partial"));
    fs::create_dir_all(&stale).expect("a crashed stage's directory");
    let bytes = tarball();
    let fetcher = rig.fetcher(None, Answers::honest());
    let got = fetcher.fetch_activation(&rec(vec![locator("kbf", &bytes)]), &facts());
    match got {
        Err(FetchError::Unavailable(why)) => assert!(why.starts_with("cannot use "), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(stale.is_dir(), "not this fetch's to remove");
}

/// An order that names one component twice is refused when the second file would be made, and
/// what the first brought is removed with the directory.
///
/// Catches a second component overwriting the first's file (the pipeline would then hash
/// another tarball than the one the child delivered).
///
/// Mutant: `create_new` replaced by `create` (red: the second pull overwrites the first and
/// the fetch goes on to the pipeline, which refuses the set instead).
#[test]
fn a_component_named_twice_is_refused_before_it_overwrites_the_first() {
    let rig = Rig::new("twice");
    let bytes = tarball();
    let ops = rig.serving(&bytes);
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    let once = locator("kbf", &bytes);
    let got = fetcher.fetch_activation(&rec(vec![once.clone(), once]), &facts());
    match got {
        Err(FetchError::Unavailable(why)) => assert!(why.starts_with("cannot create "), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
}

/// The port keeps its other implementor's behaviour: a build without a fetcher refuses the
/// activation form as it refuses the plain one.
#[test]
fn a_build_without_a_fetcher_refuses_both_forms() {
    let rec = rec(Vec::new());
    assert_eq!(NotWired.fetch(&rec), Err(FetchError::NotWired));
    assert_eq!(
        NotWired.fetch_activation(&rec, &facts()),
        Err(FetchError::NotWired)
    );
}
