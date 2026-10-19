//! The real fetcher (`kbf_installer::helper::fetcher_real::RealFetcher`) end to end on a scratch
//! host: the fetch child is the stand-in of `tests/stub/fetch_stub.rs` (it serves a file the
//! test names, or dies as told), started by `fetch_component` exactly as on a host; the verify
//! pipeline is the real one, with `gh` and the binary's `--version --json` played by a fake
//! runner. What each test proves is named on it, with the mutant that turns it red.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_host::step::Owner;
use kbf_installer::CommitId;
use kbf_installer::binary::Arch;
use kbf_installer::fetch::describe::{Answer, Described, Description};
use kbf_installer::fetch::{ChildProgram, Limits, Source};
use kbf_installer::fixtures::{
    COMMIT, FakeRunner, elf, file, me, ran, tar_gz, trusted_host_on, version_json,
};
use kbf_installer::helper::fetcher::{
    FetchError, Fetcher, InstalledRelease, NotWired, ReleaseLocator,
};
use kbf_installer::helper::fetcher_real::RealFetcher;
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::settings::Settings;
use kbf_installer::stage::{Policy, Supported, Timeouts};
use kbf_installer::testkit::{B, host_root_on};
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

/// The attestation bundle of an honest release (the fake `gh` accepts any).
const BUNDLE: &[u8] = b"{\"mediaType\":\"application/vnd.dev.sigstore.bundle+json\"}\n";

/// The entry a source lists for `name` on `platform`, as the honest source lists it: the
/// tarball `tar` and the bundle `bundle`.
fn entry_for(name: &str, platform: &str, tar: &[u8], bundle: &[u8]) -> Described {
    Described {
        name: name.to_owned(),
        platform: platform.to_owned(),
        tarball: format!("{name}-{COMMIT}-{platform}.tar.gz"),
        sha256: sha(tar),
        size: tar.len() as u64,
        bundle_sha256: sha(bundle),
        bundle_size: bundle.len() as u64,
    }
}

/// What the stand-in child writes for a description of `commit`: the form the real child writes.
fn described_as(commit: &str, components: Vec<Described>) -> String {
    Answer::Described(Description {
        commit: commit.to_owned(),
        components,
    })
    .render()
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
    /// The content of the file `gh` is given as the bundle, at each call (the fake runner
    /// reads it while the file is there).
    bundles: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Answers {
    fn honest() -> Answers {
        Answers {
            gh: (0, ""),
            probe: version_json(COMMIT, "release"),
            bundles: Arc::default(),
        }
    }

    fn runner(self) -> FakeRunner {
        FakeRunner::new(move |spawn| {
            if spawn.program.file_name().is_some_and(|n| n == "gh") {
                let at = spawn
                    .args
                    .iter()
                    .position(|a| a == "--bundle")
                    .expect("--bundle");
                let bundle = fs::read(&spawn.args[at + 1]).expect("the bundle gh is given");
                self.bundles.lock().expect("lock").push(bundle);
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
        Rig::on(name, Platform::Linux)
    }

    fn on(name: &str, platform: Platform) -> Rig {
        let (host, dir) = host_root_on(&format!("fetcher-real-{name}"), platform);
        fs::create_dir_all(dir.join("served")).expect("served");
        Rig { host, dir }
    }

    /// Put `bytes` on the shelf the stand-in child serves described releases from: under the
    /// digest it is asked for, or `describe` for the description.
    fn shelve(&self, key: &str, bytes: &[u8]) {
        let shelf = self.dir.join("shelf");
        fs::create_dir_all(&shelf).expect("shelf");
        fs::write(shelf.join(key), bytes).expect("shelved");
    }

    /// The child's operations for a shelf: serve what it is asked for from it.
    fn from_the_shelf(&self) -> PathBuf {
        self.ops(&[format!("serve:{}", self.dir.join("shelf").display())])
    }

    /// Shelve the honest release `tar` and `bundle`, and the description that lists `entries`.
    fn stock(&self, entries: Vec<Described>, tar: &[u8], bundle: &[u8]) -> PathBuf {
        self.shelve("describe", described_as(COMMIT, entries).as_bytes());
        self.shelve(&sha(tar), tar);
        self.shelve(&sha(bundle), bundle);
        self.from_the_shelf()
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
/// Mutants: `expected` passed as `None` (red: an activation without facts is refused);
/// `Pipeline::verify` skipped (red: no `VERIFIED` marker).
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
/// Mutants: a digest mismatch classed as an outage (red); the partial directory forgotten on a
/// failed pull (`std::mem::forget`; red: scratch left).
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
/// own facts reported as installed): this test, the facts test and the lease test go red.
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
/// Catches the expected facts dropped or replaced on the way (the refusal must name the field
/// and the value the order expected, not say that the order carried none), and a binary
/// installed whatever it says.
///
/// Mutants: `expected` passed as `None` (red: "the order carries no expected facts");
/// `Pipeline::verify` skipped (red).
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
        Err(FetchError::Rejected(why)) => {
            assert!(
                why.contains("protocol is 4, and the order expected 5"),
                "{why}"
            );
            assert!(why.starts_with("facts: "), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
    assert!(!rig.release().exists(), "nothing installed");
}

/// A `frontend_update` names a commit and nothing else: the source is asked what the commit holds
/// (through the child), each described file is fetched by its digest (the bundle of a component,
/// then its tarball), the bundle's text is what the attestation check is given, and the
/// pipeline installs what it accepts. This is the whole of ruling B1 on the `url`-shaped
/// stand-in; the child's own reading of the real JSON and protobuf is in `tests/fetch_e2e.rs`.
///
/// Catches the description not reaching the fetch (no component, or the wrong digests), a bundle
/// that is fetched and then not handed to `gh` (an empty attestation would be checked), and
/// scratch left behind.
///
/// Mutants: the bundle text dropped (`attestation` left empty; red: `gh` was given nothing);
/// the describe step in the root process instead of the child (`child::fetch` called in place:
/// red, as the stand-in is never asked); `plan` returning no pieces (red).
#[test]
fn a_frontend_update_is_described_fetched_verified_and_installed() {
    let rig = Rig::new("update-good");
    let tar = tarball();
    let ops = rig.stock(
        vec![entry_for("kbf", "linux-x86_64", &tar, BUNDLE)],
        &tar,
        BUNDLE,
    );
    let answers = Answers::honest();
    let seen = Arc::clone(&answers.bundles);
    let fetcher = rig.fetcher(Some(&ops), answers);
    let got = fetcher.fetch(&rec(Vec::new())).expect("installed");
    assert_eq!(got, installed());
    assert!(rig.release().join("kbf").is_file(), "the binary");
    assert!(rig.release().join("VERIFIED").is_file(), "the marker");
    assert_eq!(
        *seen.lock().expect("lock"),
        [BUNDLE.to_vec()],
        "the bundle gh checked"
    );
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
}

/// The source lists every platform (the `url` file does) and the host takes its own: another
/// platform's files are not fetched, though they are listed first and nothing serves them.
///
/// Catches the platform filter missing (the arm64 tarball would be asked for, and the stand-in
/// has none), or the first listed component taken whatever its platform.
///
/// Mutant: the platform filter removed in `Description::select` (red: kbf twice, or a refusal
/// of the arm64 entry).
#[test]
fn only_the_hosts_platform_is_fetched() {
    let rig = Rig::new("update-platform");
    let tar = tarball();
    let other = elf(0xb7, "arm payload");
    let other_bundle = b"{\"other\":true}\n";
    let ops = rig.stock(
        vec![
            entry_for("kbf", "linux-arm64", &other, other_bundle),
            entry_for("kbf", "linux-x86_64", &tar, BUNDLE),
        ],
        &tar,
        BUNDLE,
    );
    let answers = Answers::honest();
    let seen = Arc::clone(&answers.bundles);
    let fetcher = rig.fetcher(Some(&ops), answers);
    assert_eq!(fetcher.fetch(&rec(Vec::new())), Ok(installed()));
    assert_eq!(*seen.lock().expect("lock"), [BUNDLE.to_vec()]);
}

/// A source that says it will not describe the commit (unknown, or not complete) is a release
/// that could not be had, in the source's words and under the word `fetch` (an `Unavailable`:
/// the frontend's cycle words that `fetch` and a `Rejected` `stage`); nothing is made on the
/// host, and no file is asked for.
///
/// Catches the refusal taken for a verdict on what arrived (the frontend would report `stage`),
/// the source's words lost, and scratch made before the answer is known.
///
/// Mutant: `FetchError::Describe` classed with the verdicts (`Rejected`; red).
#[test]
fn a_commit_the_source_will_not_describe_is_refused_with_its_words() {
    for said in [
        "NotFound: no tarball of the commit is stored",
        "FailedPrecondition: linux-x86_64 has no attestation of kbf",
    ] {
        let rig = Rig::new("update-refused");
        rig.shelve("describe", format!("refused {said}\n").as_bytes());
        let ops = rig.from_the_shelf();
        let fetcher = rig.fetcher(Some(&ops), Answers::honest());
        assert_eq!(
            fetcher.fetch(&rec(Vec::new())),
            Err(FetchError::Unavailable(format!(
                "the release source will not describe it: {said}"
            )))
        );
        assert!(!rig.work().exists(), "nothing was made");
    }
}

/// A description that is not complete for the host (only another platform's component), of
/// another commit, or with a component the host cannot use, is refused with what is wrong
/// (`fetch`, as a source's refusal is) and nothing is made.
///
/// Catches an incomplete or foreign description reaching the pipeline (a release with no binary
/// for this host, or another release installed under this commit's name).
///
/// Mutants: the completeness check removed (red: no refusal, then an empty set reaches the
/// pipeline); the commit check removed (red).
#[test]
fn a_description_that_is_not_the_hosts_release_is_refused() {
    let tar = tarball();
    let wrong_commit = {
        let rig = Rig::new("update-wrong-commit");
        rig.shelve(
            "describe",
            described_as(B, vec![entry_for("kbf", "linux-x86_64", &tar, BUNDLE)]).as_bytes(),
        );
        (rig.from_the_shelf(), rig)
    };
    let only_arm = {
        let rig = Rig::new("update-only-arm");
        rig.shelve(
            "describe",
            described_as(COMMIT, vec![entry_for("kbf", "linux-arm64", &tar, BUNDLE)]).as_bytes(),
        );
        (rig.from_the_shelf(), rig)
    };
    let cases = [
        (
            wrong_commit,
            format!("it describes commit {B}, and the order names {COMMIT}"),
        ),
        (only_arm, "it has no kbf for linux-x86_64".to_owned()),
    ];
    for ((ops, rig), why) in cases {
        let fetcher = rig.fetcher(Some(&ops), Answers::honest());
        assert_eq!(
            fetcher.fetch(&rec(Vec::new())),
            Err(FetchError::Unavailable(format!(
                "the description is not usable: {why}"
            )))
        );
        assert!(!rig.work().exists(), "nothing was made");
    }
}

/// The source's word about a digest is a claim: bytes that do not hash to the digest the
/// description gave are refused as `hash`, for the tarball and for the bundle alike, and what
/// was made goes. (The attestation check passes any bundle here, so only the digest of the
/// bundle stands between a substituted one and the pipeline.)
///
/// Catches a described digest trusted without hashing what arrived (the tarball, which the
/// pipeline would catch later with another word, and the bundle, which nothing else checks).
///
/// Mutants: the digest comparison removed from `fetch_component` (red: the tarball case reports
/// the pipeline's refusal, the bundle case installs); the bundle pulled with the digest of what
/// arrived (red: installed).
#[test]
fn bytes_that_do_not_match_the_described_digest_are_refused() {
    let tar = tarball();
    let flipped = |bytes: &[u8]| {
        let mut wrong = bytes.to_vec();
        *wrong.last_mut().expect("bytes") ^= 0xff;
        wrong
    };
    for bundle_is_wrong in [false, true] {
        let rig = Rig::new(&format!("update-digest-{bundle_is_wrong}"));
        let entry = entry_for("kbf", "linux-x86_64", &tar, BUNDLE);
        rig.shelve("describe", described_as(COMMIT, vec![entry]).as_bytes());
        let served_tar = if bundle_is_wrong {
            tar.clone()
        } else {
            flipped(&tar)
        };
        let served_bundle = if bundle_is_wrong {
            flipped(BUNDLE)
        } else {
            BUNDLE.to_vec()
        };
        rig.shelve(&sha(&tar), &served_tar);
        rig.shelve(&sha(BUNDLE), &served_bundle);
        let ops = rig.from_the_shelf();
        let fetcher = rig.fetcher(Some(&ops), Answers::honest());
        match fetcher.fetch(&rec(Vec::new())) {
            Err(FetchError::Rejected(why)) => {
                assert!(why.starts_with("hash: the download has sha256 "), "{why}");
                let want = if bundle_is_wrong {
                    sha(BUNDLE)
                } else {
                    sha(&tar)
                };
                assert!(why.ends_with(&format!("the order names {want}")), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
        assert!(!rig.release().exists(), "nothing installed");
    }
}

/// A bundle that is not text cannot be the attestation text the pipeline takes; it is refused as
/// `attestation`, not installed and not passed on mangled.
///
/// Catches a lossy conversion of the bundle (the replacement characters would be checked in its
/// place).
///
/// Mutant: `read_to_string` replaced by a lossy read (red: installed).
#[test]
fn a_bundle_that_is_not_text_is_refused() {
    let rig = Rig::new("update-binary-bundle");
    let tar = tarball();
    let bundle = [0xff, 0xfe, 0x00, b'{', b'}', b'\n'];
    let ops = rig.stock(
        vec![entry_for("kbf", "linux-x86_64", &tar, &bundle)],
        &tar,
        &bundle,
    );
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    match fetcher.fetch(&rec(Vec::new())) {
        Err(FetchError::Rejected(why)) => {
            assert!(
                why.starts_with("attestation: the bundle of kbf cannot be read as text: "),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.work_left(), Vec::<String>::new(), "no scratch");
}

/// A source that cannot be asked (the child will not start, or ends without a word) is an outage
/// that makes nothing: the describe goes through the child, not around it.
///
/// Catches the describe step done in the root process (a program path that does not exist would
/// not matter to it), and a silent child taken for an empty answer.
///
/// Mutant: the describe done in-process (red: no "cannot start" message).
#[test]
fn a_source_that_cannot_be_asked_is_an_outage_and_makes_nothing() {
    let rig = Rig::new("update-outage");
    let fetcher = rig.fetcher(None, Answers::honest());
    match fetcher.fetch(&rec(Vec::new())) {
        Err(FetchError::Unavailable(why)) => {
            assert!(why.starts_with("cannot start the fetch child"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!rig.work().exists(), "nothing was made");
    // The child runs and has nothing to serve: it says why on standard error and ends.
    let ops = rig.from_the_shelf();
    let fetcher = rig.fetcher(Some(&ops), Answers::honest());
    assert_eq!(
        fetcher.fetch(&rec(Vec::new())),
        Err(FetchError::Unavailable(
            "the child ended without sending a byte (fetch-child: nothing is served as describe)"
                .to_owned()
        ))
    );
    assert!(!rig.work().exists(), "nothing was made");
}

/// A host no release is built for (a Mac on x86-64) is told so before the source is asked.
///
/// Catches a request for a platform that does not exist (the source would answer with a
/// different complaint, or the host would install another platform's binary).
#[test]
fn a_host_no_release_is_built_for_is_refused_before_anything_is_asked() {
    let rig = Rig::on("update-mac-x86", Platform::MacOs);
    let fetcher = rig.fetcher(None, Answers::honest());
    assert_eq!(
        fetcher.fetch(&rec(Vec::new())),
        Err(FetchError::Rejected(
            "platform: no release is built for macos on x86_64".to_owned()
        ))
    );
    assert!(!rig.work().exists(), "nothing was made");
}

/// An activation that names no component is refused (the cycle never sends one, but the
/// fetcher does not rely on that, and it does not ask the source: an activation's components
/// come from its offer).
///
/// Mutant: the empty-components check removed (red: a panic in the pipeline, or a describe).
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
/// Mutant: the plain-name check removed (red).
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
/// Mutant: `Pipeline::verify` skipped (red: the release is installed).
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
