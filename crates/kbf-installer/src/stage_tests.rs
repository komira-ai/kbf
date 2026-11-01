use std::ffi::CString;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Instant;

use kbf_host::path::HostPath;
use kbf_host::testkit::FakeExec;

use super::*;
use crate::fixtures::{COMMIT, FakeRunner, OTHER, elf, file, me, ran, tar_gz, trusted_host_on};
use crate::fixtures::{Item, version_json};
use crate::helper::leases::fake::FakeLeases;
use crate::run::{Ran, SystemRunner};
use crate::testkit::host_root;

fn commit() -> CommitId {
    CommitId::new(COMMIT).expect("commit")
}

fn owner() -> Owner {
    let me = me();
    Owner {
        uid: me.uid,
        gid: me.gid,
    }
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

fn policy() -> Policy {
    Policy {
        max_component_bytes: 1 << 20,
        max_binary_bytes: 1 << 20,
        min_protocol: 0,
        supported: Supported {
            protocol: 10,
            min_protocol: 10,
            state_schema: 10,
        },
        timeouts: Timeouts::DEFAULT,
    }
}

/// The tarball of an honest x86-64 Linux `kbf`.
fn tarball() -> Vec<u8> {
    tar_gz(&[file("kbf", &elf(0x3e, "payload"))])
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A runner that answers `gh` with `gh` and the probe with `probe`.
fn runner(gh: Ran, probe: Ran) -> FakeRunner {
    FakeRunner::new(move |spawn| {
        if spawn.program.file_name().is_some_and(|n| n == "gh") {
            Ok(gh.clone())
        } else {
            Ok(probe.clone())
        }
    })
}

fn honest() -> FakeRunner {
    runner(ran(0, "", ""), ran(0, &version_json(COMMIT, "release"), ""))
}

fn probing(stdout: &str) -> FakeRunner {
    runner(ran(0, "", ""), ran(0, stdout, ""))
}

/// The names of the programs a runner was asked to run, in order.
fn programs(runner: &FakeRunner) -> Vec<String> {
    runner
        .calls()
        .iter()
        .map(|c| {
            c.program
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into()
        })
        .collect()
}

struct Rig {
    host: Layout,
    dir: PathBuf,
    settings: Settings,
    trust: FakeExec,
    policy: Policy,
    leases: FakeLeases,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let (host, dir) = host_root(&format!("stage-{name}"));
        Rig::on(host, dir)
    }

    fn on(host: Layout, dir: PathBuf) -> Rig {
        let mut settings = Settings::defaults(&host);
        settings.run_as = me();
        settings.arch = Arch::X86_64;
        let trust = trusted_host_on(host.platform());
        Rig {
            host,
            dir,
            settings,
            trust,
            policy: policy(),
            leases: FakeLeases::default(),
        }
    }

    /// The fetch step's hand-over of `bytes` as the tarball of `kbf`, and the order's
    /// locator for them.
    fn fetch(&self, bytes: &[u8]) -> (Locator, Fetched) {
        let partial = Partial::create(&self.host, &commit(), owner()).expect("partial");
        let path = partial.dl().join("kbf.tar.gz");
        fs::write(&path, bytes).expect("seed");
        let locator = Locator {
            name: "kbf".to_owned(),
            sha256: sha(bytes),
            size: bytes.len() as u64,
            attestation: "{\"mediaType\":\"bundle\"}\n".to_owned(),
        };
        let received = Received {
            name: "kbf".to_owned(),
            tarball: path,
            bytes: bytes.len() as u64,
            sha256: sha(bytes),
        };
        let fetched = Fetched {
            partial,
            files: vec![received],
        };
        (locator, fetched)
    }

    fn verify(
        &self,
        runner: &dyn Runner,
        kind: OrderKind,
        locators: &[Locator],
        expected: Option<&Facts>,
        fetched: Fetched,
    ) -> Result<Staged, Refusal> {
        let commit = commit();
        let pipeline = Pipeline {
            host: &self.host,
            settings: &self.settings,
            policy: self.policy,
            trust: &self.trust,
            runner,
            leases: &self.leases,
            owner: owner(),
        };
        let order = Order {
            kind,
            commit: &commit,
            components: locators,
            expected,
        };
        pipeline.verify(&order, fetched)
    }

    /// An activation of the honest release, with `runner` playing the programs.
    fn activate(&self, runner: &dyn Runner) -> Result<Staged, Refusal> {
        let (locator, fetched) = self.fetch(&tarball());
        self.verify(
            runner,
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
    }

    fn release(&self) -> PathBuf {
        self.host.join(&self.host.release_dir(&commit()))
    }

    fn work(&self) -> PathBuf {
        self.host.join(&self.host.work_dir())
    }

    /// Nothing of the stage is left: no release directory, no scratch.
    fn assert_nothing_installed(&self) {
        assert!(!self.release().exists(), "a release directory was made");
        let left: Vec<_> = fs::read_dir(self.work())
            .map(|d| d.map(|e| e.expect("entry").file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "scratch left behind: {left:?}");
    }
}

/// The scratch root holds an executable (the installed release), which `cargo llvm-cov`
/// would scan as an object: it goes with the rig.
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[path = "stage_limits_tests.rs"]
mod limits;

#[path = "stage_more_tests.rs"]
mod more;

/// Catches a stage that does not do the whole job, in order: the honest release is checked
/// by `gh` (with the unprivileged user, a scratch home and the order's commit) and then
/// run once, as the unprivileged user with an empty environment and a 10 s limit, and is
/// installed as `releases/<commit>/` holding the 0755 binary and the `VERIFIED` marker,
/// root's and with nothing of the download left. The scratch directories are gone, and the
/// cycle's view of the release is what the binary declared. (The state is left alone:
/// `the_pipeline_installs_and_never_writes_the_state`.)
///
/// Catches: the probe run with no `run_as`; the marker not written; `dl/` left in
/// the release.
#[test]
fn an_honest_release_is_verified_in_order_and_installed_with_its_marker() {
    let rig = Rig::new("honest");
    let runner = honest();
    let staged = rig.activate(&runner).expect("staged");

    assert_eq!(programs(&runner), ["gh", "kbf"], "gh before anything runs");
    let calls = runner.calls();
    let probe = &calls[1];
    assert_eq!(probe.args, ["--version", "--json"]);
    assert!(probe.env.is_empty());
    assert_eq!(probe.run_as, Some(me()));
    assert_eq!(probe.timeout, Duration::from_secs(10));
    assert_eq!(calls[0].timeout, Duration::from_secs(120));
    let gh = &calls[0].args;
    assert!(
        gh.iter().any(|a| a == COMMIT),
        "--source-digest is the commit"
    );

    let release = rig.release();
    assert_eq!(
        fs::read(release.join("kbf")).expect("binary"),
        elf(0x3e, "payload")
    );
    let mode = |p: &str| fs::metadata(release.join(p)).expect(p).permissions().mode() & 0o7777;
    assert_eq!(mode("kbf"), 0o755);
    assert_eq!(mode("VERIFIED"), 0o644);
    assert_eq!(
        fs::read_to_string(release.join("VERIFIED")).expect("m"),
        format!("{COMMIT}\n")
    );
    let mut names: Vec<_> = fs::read_dir(&release)
        .expect("dir")
        .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["VERIFIED", "kbf"],
        "the download is not part of the release"
    );
    assert_eq!(mode_of_dir(&release), 0o755);

    assert_eq!(staged.commit, commit());
    assert_eq!(staged.components, ["kbf"]);
    assert_eq!(staged.source_ref.as_str(), "refs/heads/main");
    assert_eq!(staged.platform, "linux-x86_64");
    assert!(!staged.already_installed);
    let installed = staged.installed();
    assert_eq!(
        (
            installed.protocol,
            installed.min_protocol,
            installed.state_schema
        ),
        (4, 3, 7)
    );
    assert_eq!(installed.profile, "release");

    let left: Vec<_> = fs::read_dir(rig.work()).expect("work").collect();
    assert!(left.is_empty(), "scratch left: {left:?}");
}

pub(super) fn mode_of_dir(dir: &Path) -> u32 {
    fs::metadata(dir).expect("dir").permissions().mode() & 0o7777
}

/// Catches a size that is only claimed: a tarball one byte short or long on disk, a fetch
/// parent that counted another number of bytes, and a component over the host's cap each
/// end the stage as `size` before `gh` is asked or anything is unpacked.
///
/// Catches: the on-disk length not compared; the received count not compared;
/// the cap not applied.
#[test]
fn a_component_of_another_size_than_ordered_is_refused_before_anything_runs() {
    let bytes = tarball();
    for case in ["short", "long", "counted", "cap"] {
        let mut rig = Rig::new(&format!("size-{case}"));
        let (mut locator, mut fetched) = rig.fetch(&bytes);
        match case {
            "short" => locator.size += 1,
            "long" => locator.size -= 1,
            "counted" => fetched.files[0].bytes += 1,
            _ => rig.policy.max_component_bytes = locator.size - 1,
        }
        let runner = honest();
        let err = rig
            .verify(
                &runner,
                OrderKind::Activate,
                &[locator],
                Some(&facts()),
                fetched,
            )
            .expect_err(case);
        assert_eq!(err.word(), Some("size"), "{case}: {err}");
        assert!(runner.calls().is_empty(), "{case}: nothing was run");
        rig.assert_nothing_installed();
    }
}

/// Catches a digest that is only a locator: the order's sha256 must equal what the fetch
/// parent computed, and what is on the disk now (a tarball swapped after it was hashed has
/// the right length and the wrong hash). Neither reaches `gh`.
///
/// Catches: the claim not compared with the order; the file not hashed again.
#[test]
fn a_digest_other_than_the_orders_is_refused() {
    let bytes = tarball();
    for case in ["order", "swapped"] {
        let rig = Rig::new(&format!("hash-{case}"));
        let (mut locator, fetched) = rig.fetch(&bytes);
        if case == "order" {
            locator.sha256 = sha(b"another file");
        } else {
            let mut other = bytes.clone();
            let last = other.len() - 1;
            other[last] ^= 1;
            fs::write(&fetched.files[0].tarball, other).expect("swap");
        }
        let runner = honest();
        let err = rig
            .verify(
                &runner,
                OrderKind::Activate,
                &[locator],
                Some(&facts()),
                fetched,
            )
            .expect_err(case);
        assert_eq!(err.word(), Some("hash"), "{case}: {err}");
        assert!(runner.calls().is_empty());
        rig.assert_nothing_installed();
    }
}

/// Catches a file that is not what the fetch parent made: a link in its place, a file with
/// a second name, a FIFO (which must not block the open), another owner's file. Each is a
/// `trust_chain` refusal, before a byte is read.
///
/// Catches: `O_NOFOLLOW` dropped; the link-count check dropped; `O_NONBLOCK`
/// dropped (the FIFO case hangs, so it is bounded by the test runner's timeout); the owner
/// check dropped.
#[test]
fn a_download_that_is_not_the_parents_plain_file_is_refused() {
    let bytes = tarball();
    for case in ["symlink", "hardlink", "fifo", "owner"] {
        let rig = Rig::new(&format!("file-{case}"));
        let (locator, fetched) = rig.fetch(&bytes);
        let path = fetched.files[0].tarball.clone();
        let real = path.with_extension("real");
        match case {
            "symlink" => {
                fs::rename(&path, &real).expect("move");
                symlink(&real, &path).expect("link");
            }
            "hardlink" => fs::hard_link(&path, &real).expect("link"),
            "fifo" => {
                fs::remove_file(&path).expect("rm");
                let c = CString::new(path.to_str().expect("utf-8")).expect("c");
                // SAFETY: mkfifo reads a NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }
            // The file is intact; the stage expects another owner than this process.
            _ => {}
        }
        let runner = honest();
        let commit = commit();
        let pipeline = Pipeline {
            host: &rig.host,
            settings: &rig.settings,
            policy: rig.policy,
            trust: &rig.trust,
            runner: &runner,
            leases: &rig.leases,
            owner: Owner {
                uid: if case == "owner" {
                    owner().uid + 1
                } else {
                    owner().uid
                },
                gid: owner().gid,
            },
        };
        let order = Order {
            kind: OrderKind::Activate,
            commit: &commit,
            components: &[locator],
            expected: Some(&facts()),
        };
        let err = pipeline.verify(&order, fetched).expect_err(case);
        assert_eq!(err.word(), Some("trust_chain"), "{case}: {err}");
        assert!(runner.calls().is_empty(), "{case}");
        rig.assert_nothing_installed();
    }
}

/// Catches the attestation being skipped or ignored: a `gh` that refuses ends the stage as
/// `attestation`, every allowed ref is tried before it does, nothing is unpacked or run
/// (the binary is never started), and nothing is installed.
///
/// Catches: `gh` not run; its exit status ignored.
#[test]
fn a_release_gh_does_not_accept_is_refused_and_nothing_runs() {
    let mut rig = Rig::new("gh-no");
    rig.settings.source_refs = ["refs/heads/main", "refs/heads/release"]
        .iter()
        .map(|r| SourceRef::new(r).expect("ref"))
        .collect();
    let runner = runner(
        ran(1, "", "no matching attestation\n"),
        ran(0, &version_json(COMMIT, "release"), ""),
    );
    let err = rig.activate(&runner).expect_err("not attested");
    assert_eq!(err.word(), Some("attestation"), "{err}");
    assert_eq!(
        programs(&runner),
        ["gh", "gh", "gh", "gh"],
        "each allowed ref under each accepted signer, then stop"
    );
    rig.assert_nothing_installed();
}

/// Catches the order of the checks, which is the safety property: an archive that is
/// garbage and is also not attested is refused as `attestation` (it was never opened), and
/// an attested archive that is garbage is refused as `shape` without anything being run.
///
/// Catches: the archive unpacked before the attestation is asked.
#[test]
fn nothing_is_unpacked_before_the_attestation_holds() {
    let rig = Rig::new("order");
    let garbage = b"this is not a tarball".to_vec();
    let (locator, fetched) = rig.fetch(&garbage);
    let runner = runner(ran(1, "", "bad"), ran(0, "", ""));
    let err = rig
        .verify(
            &runner,
            OrderKind::Activate,
            std::slice::from_ref(&locator),
            Some(&facts()),
            fetched,
        )
        .expect_err("refused");
    assert_eq!(err.word(), Some("attestation"), "{err}");
    rig.assert_nothing_installed();

    let (locator, fetched) = rig.fetch(&garbage);
    let runner = honest();
    let err = rig
        .verify(
            &runner,
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("refused");
    assert_eq!(err.word(), Some("shape"), "{err}");
    assert_eq!(programs(&runner), ["gh"], "the garbage was never run");
    rig.assert_nothing_installed();
}

/// Catches an archive that is accepted though it is not one plain file: a link entry, a
/// second entry, a path in the name, trailing bytes. Each is `shape`, after `gh` accepted
/// it and before anything ran.
///
/// Catches: the shape check dropped from the pipeline (the symlink entry installs).
#[test]
fn an_attested_archive_of_the_wrong_shape_is_refused_unrun() {
    let link = Item {
        name: b"kbf",
        kind: b'2',
        data: b"x",
    };
    let mut trailing = tarball();
    trailing.extend_from_slice(b"extra");
    let cases = [
        ("link", tar_gz(&[link])),
        (
            "two",
            tar_gz(&[file("kbf", &elf(0x3e, "")), file("other", b"x")]),
        ),
        ("path", tar_gz(&[file("bin/kbf", &elf(0x3e, ""))])),
        ("trailing", trailing),
    ];
    for (name, bytes) in cases {
        let rig = Rig::new(&format!("shape-{name}"));
        let (locator, fetched) = rig.fetch(&bytes);
        let runner = honest();
        let err = rig
            .verify(
                &runner,
                OrderKind::Activate,
                &[locator],
                Some(&facts()),
                fetched,
            )
            .expect_err(name);
        assert_eq!(err.word(), Some("shape"), "{name}: {err}");
        assert_eq!(programs(&runner), ["gh"], "{name}: nothing was run");
        rig.assert_nothing_installed();
    }
}

/// Catches an attested binary of another platform being run: a 64-bit ARM ELF on an x86-64
/// host, a Mach-O on Linux and a script are `platform` refusals, before the probe.
///
/// Catches: the header check dropped.
#[test]
fn a_binary_for_another_platform_is_refused_unrun() {
    for (name, content) in [
        ("arm", elf(0xb7, "")),
        ("macho", crate::fixtures::macho(0x0100_000c, "")),
        ("script", b"#!/usr/bin/true\necho hi\n".to_vec()),
    ] {
        let rig = Rig::new(&format!("platform-{name}"));
        let (locator, fetched) = rig.fetch(&tar_gz(&[file("kbf", &content)]));
        let runner = honest();
        let err = rig
            .verify(
                &runner,
                OrderKind::Activate,
                &[locator],
                Some(&facts()),
                fetched,
            )
            .expect_err(name);
        assert_eq!(err.word(), Some("platform"), "{name}: {err}");
        assert_eq!(programs(&runner), ["gh"]);
        rig.assert_nothing_installed();
    }
}

/// Catches the expected facts being compared loosely: each of the five (platform, protocol,
/// min_protocol, state_schema, profile) that differs from what the binary declared is a
/// `facts` refusal naming the field, and the release is not installed.
///
/// Catches: one field dropped from the comparison (each field is tried alone).
#[test]
fn each_expected_fact_must_equal_what_the_binary_declares() {
    for field in [
        "platform",
        "protocol",
        "min_protocol",
        "state_schema",
        "profile",
    ] {
        let rig = Rig::new(&format!("facts-{field}"));
        let mut expected = facts();
        match field {
            "platform" => expected.platform = "linux-arm64".to_owned(),
            "protocol" => expected.protocol += 1,
            "min_protocol" => expected.min_protocol += 1,
            "state_schema" => expected.state_schema += 1,
            _ => expected.profile = "debug".to_owned(),
        }
        let (locator, fetched) = rig.fetch(&tarball());
        let err = rig
            .verify(
                &honest(),
                OrderKind::Activate,
                &[locator],
                Some(&expected),
                fetched,
            )
            .expect_err(field);
        assert_eq!(err.word(), Some("facts"), "{field}: {err}");
        assert!(
            err.to_string().contains(field) || field == "platform",
            "{err}"
        );
        rig.assert_nothing_installed();
    }
}

/// Catches facts that fail open: an activation with no expected facts is refused as
/// `facts` before any work (nothing is run, nothing created), while a `frontend_update`
/// may leave them out and is then held to the commit and the release profile only.
///
/// Catches: a missing `expected` skipping the comparison for an activation; the
/// commit check skipped for a `frontend_update`.
#[test]
fn facts_are_required_for_an_activation_and_optional_for_a_frontend_update() {
    let rig = Rig::new("facts-optional");
    let (locator, fetched) = rig.fetch(&tarball());
    let runner = honest();
    let err = rig
        .verify(&runner, OrderKind::Activate, &[locator], None, fetched)
        .expect_err("fails closed");
    assert_eq!(err.word(), Some("facts"));
    assert_eq!(err.code(), "facts-missing");
    assert!(runner.calls().is_empty());
    rig.assert_nothing_installed();

    let (locator, fetched) = rig.fetch(&tarball());
    rig.verify(
        &honest(),
        OrderKind::FrontendUpdate,
        &[locator],
        None,
        fetched,
    )
    .expect("a frontend update needs no expected facts");

    let rig = Rig::new("facts-optional-commit");
    let (locator, fetched) = rig.fetch(&tarball());
    let other = probing(&version_json(OTHER, "release"));
    let err = rig
        .verify(&other, OrderKind::FrontendUpdate, &[locator], None, fetched)
        .expect_err("another commit");
    assert_eq!(err.word(), Some("commit_or_ref"), "{err}");
    rig.assert_nothing_installed();
}

/// Catches a probe read loosely or run unbounded: output over 4 KiB, a field named twice,
/// a non-zero exit and a build that declares another platform than its own header are
/// each refused (`facts`); a build that declares the right one passes, and so does one
/// carrying a field this installer does not know (a newer build's).
///
/// Catches: the cap dropped; the declared platform not compared with the header;
/// unknown fields refused again (`probe-unknown` fails).
#[test]
fn the_probe_is_read_strictly() {
    let json = version_json(COMMIT, "release");
    let big = format!("{json}{}", " ".repeat(5000));
    let unknown = json.replace('}', ",\"extra\":1}");
    let twice = json.replace('}', ",\"profile\":\"release\"}");
    let lies = json.replace('}', ",\"platform\":\"linux-arm64\"}");
    let truth = json.replace('}', ",\"platform\":\"linux-x86_64\"}");
    for (name, runner) in [
        ("big", probing(&big)),
        ("twice", probing(&twice)),
        ("lies", probing(&lies)),
        ("exit", runner(ran(0, "", ""), ran(3, &json, ""))),
        ("empty", probing("")),
    ] {
        let rig = Rig::new(&format!("probe-{name}"));
        let err = rig.activate(&runner).expect_err(name);
        assert_eq!(err.word(), Some("facts"), "{name}: {err}");
        rig.assert_nothing_installed();
    }
    let rig = Rig::new("probe-truth");
    rig.activate(&probing(&truth))
        .expect("a declared platform that is right");
    // A field this installer does not know is a newer build's, not a lie: ignored.
    let rig = Rig::new("probe-unknown");
    rig.activate(&probing(&unknown))
        .expect("a field the installer does not know");
}

/// Catches the operator's floor and the installer's own limits being skipped: a release
/// whose protocol is below `min_protocol` is `floor`; one whose protocol, min_protocol or
/// state schema is above what this installer understands is `installer_too_old`.
///
/// Catches: the floor comparison reversed; one of the three limits dropped.
#[test]
fn a_release_below_the_floor_or_beyond_the_installer_is_refused() {
    let mut rig = Rig::new("floor");
    rig.policy.min_protocol = 5;
    let err = rig.activate(&honest()).expect_err("below");
    assert_eq!(err.word(), Some("floor"), "{err}");
    rig.assert_nothing_installed();
    rig.policy.min_protocol = 4;
    rig.activate(&honest()).expect("at the floor");

    for field in ["protocol", "min_protocol", "state_schema"] {
        let mut rig = Rig::new(&format!("old-{field}"));
        match field {
            "protocol" => rig.policy.supported.protocol = 3,
            "min_protocol" => rig.policy.supported.min_protocol = 2,
            _ => rig.policy.supported.state_schema = 6,
        }
        let err = rig.activate(&honest()).expect_err(field);
        assert_eq!(err.word(), Some("installer_too_old"), "{field}: {err}");
        assert!(err.to_string().contains(field), "{err}");
        rig.assert_nothing_installed();
    }
}

/// Catches a lease check that is missing, late or read from the wrong place: a live lease,
/// and a source that cannot tell, each refuse as `lease` after the binary was verified and
/// before the release directory exists.
///
/// Catches: the check dropped; the check after the rename.
#[test]
fn a_live_lease_refuses_before_the_release_is_installed() {
    let rig = Rig::new("lease");
    rig.leases.set(2);
    let runner = honest();
    let err = rig.activate(&runner).expect_err("busy");
    assert_eq!(err.word(), Some("lease"), "{err}");
    assert!(err.to_string().contains("2 leases are live"));
    assert_eq!(programs(&runner), ["gh", "kbf"], "it was verified first");
    rig.assert_nothing_installed();

    rig.leases.set(usize::MAX);
    let err = rig.activate(&honest()).expect_err("cannot tell");
    assert_eq!(err.word(), Some("lease"));
    assert!(err.to_string().contains("cannot tell"));
    rig.assert_nothing_installed();

    rig.leases.set(0);
    rig.activate(&honest()).expect("idle");
}

/// Catches the marker being written after the rename: with the marker's name taken by a
/// directory in the partial tree, writing it fails, and then no release directory may
/// exist (a `releases/<commit>/` is only ever made whole, with its marker).
///
/// Catches: the rename before the marker.
#[test]
fn the_marker_is_written_before_the_directory_is_renamed_into_place() {
    let rig = Rig::new("marker");
    let (locator, fetched) = rig.fetch(&tarball());
    fs::create_dir(fetched.partial.path().join(VERIFIED)).expect("plant");
    let err = rig
        .verify(
            &honest(),
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("cannot write the marker");
    assert_eq!(err.code(), "work-dir");
    rig.assert_nothing_installed();
    assert!(
        crate::state::read_unlocked(&rig.host)
            .expect("state")
            .is_none()
    );
}

/// Catches a directory being taken as proof: the same release verified again is installed
/// "already" (and its bytes are not touched); a directory there without the marker, with
/// other bytes, or as a link is `release-differs` and left as it was. A new fetch is
/// always fully verified first (the honest runner is asked every time).
///
/// Catches: the marker not required; the bytes not compared; a component missing
/// from the directory taken as the same.
#[test]
fn an_existing_release_directory_is_the_same_release_or_a_refusal() {
    let rig = Rig::new("existing");
    let first = rig.activate(&honest()).expect("first");
    assert!(!first.already_installed);
    let runner = honest();
    let again = rig.activate(&runner).expect("again");
    assert!(again.already_installed);
    assert_eq!(
        programs(&runner),
        ["gh", "kbf"],
        "verified again, not skipped"
    );
    assert_eq!(fs::read_dir(rig.work()).expect("work").count(), 0);

    let kbf = rig.release().join("kbf");
    fs::write(&kbf, b"different bytes").expect("tamper");
    let err = rig.activate(&honest()).expect_err("other bytes");
    assert_eq!(err.code(), "release-differs");
    assert_eq!(fs::read(&kbf).expect("kept"), b"different bytes");
    assert_eq!(err.word(), None);

    let rig = Rig::new("existing-nomarker");
    fs::create_dir_all(rig.release()).expect("dir");
    fs::write(rig.release().join("kbf"), elf(0x3e, "payload")).expect("bytes");
    let err = rig.activate(&honest()).expect_err("no marker");
    assert_eq!(err.code(), "release-differs");
    assert!(!rig.release().join(VERIFIED).exists());

    // The marker is there and the binary is not: still a different release.
    let rig = Rig::new("existing-nofile");
    fs::create_dir_all(rig.release()).expect("dir");
    fs::write(rig.release().join(VERIFIED), b"").expect("marker");
    let err = rig.activate(&honest()).expect_err("no binary");
    assert_eq!(err.code(), "release-differs");

    let rig = Rig::new("existing-link");
    fs::create_dir_all(rig.release().parent().expect("parent")).expect("dir");
    symlink("/", rig.release()).expect("link");
    let err = rig.activate(&honest()).expect_err("a link");
    assert_eq!(err.code(), "release-differs");
}

/// Catches a component set that is not the one ordered: a file missing, one fetched twice
/// and one nobody ordered are each `fetch`.
#[test]
fn the_files_fetched_must_be_the_components_ordered() {
    let rig = Rig::new("components");
    let (locator, mut fetched) = rig.fetch(&tarball());
    let extra = Received {
        name: "kbf-daemon".to_owned(),
        ..fetched.files[0].clone()
    };
    fetched.files.push(extra);
    let err = rig
        .verify(
            &honest(),
            OrderKind::Activate,
            std::slice::from_ref(&locator),
            Some(&facts()),
            fetched,
        )
        .expect_err("extra");
    assert_eq!(err.word(), Some("fetch"), "{err}");

    let (locator, mut fetched) = rig.fetch(&tarball());
    fetched.files.clear();
    let err = rig
        .verify(
            &honest(),
            OrderKind::Activate,
            std::slice::from_ref(&locator),
            Some(&facts()),
            fetched,
        )
        .expect_err("missing");
    assert!(err.to_string().contains("was not fetched"), "{err}");

    let (locator, mut fetched) = rig.fetch(&tarball());
    let again = fetched.files[0].clone();
    fetched.files.push(again);
    let err = rig
        .verify(
            &honest(),
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("twice");
    assert!(err.to_string().contains("twice"), "{err}");
}

/// Catches the trust list not being checked first: a `gh` that group or others may write
/// ends the stage as `trust_chain` before any file of the download is opened.
///
/// Catches: the trust check dropped.
#[test]
fn an_untrusted_gh_ends_the_stage_first() {
    let mut rig = Rig::new("trust");
    let gh = HostPath::new(crate::fixtures::GH_PATH).expect("path");
    if let Some(kbf_host::exec::Entry::File { perms, .. }) = rig.trust.entries.get_mut(&gh) {
        *perms = kbf_host::step::Perms::new(0o775);
    }
    let runner = honest();
    let err = rig.activate(&runner).expect_err("untrusted");
    assert_eq!(err.word(), Some("trust_chain"), "{err}");
    assert!(runner.calls().is_empty());
}

/// Catches an attestation bundle over the cap being handed to `gh`: 256 KiB is the most.
#[test]
fn an_oversize_attestation_bundle_is_refused_before_gh() {
    let rig = Rig::new("bundle");
    let (mut locator, fetched) = rig.fetch(&tarball());
    locator.attestation = "x".repeat(BUNDLE_CAP as usize + 1);
    let runner = honest();
    let err = rig
        .verify(
            &runner,
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("big");
    assert_eq!(err.word(), Some("size"));
    assert!(runner.calls().is_empty());
}
