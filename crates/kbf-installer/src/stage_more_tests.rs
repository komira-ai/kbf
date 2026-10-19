//! More of the stage's tests: a host that is not x86-64 Linux, an order of two components,
//! and the rig's own hygiene. They share `stage_tests`' rig.

use kbf_host::path::Root;

use super::*;
use crate::fixtures::macho;
use crate::state::State;

const ARM64_MACHO: u32 = 0x0100_000c;

/// The tool lines of `codesign --display` for a signature CI makes.
const SIGNED: &str = "CodeDirectory v=20400 size=1 flags=0x10002(adhoc,runtime) hashes=1+2\n";

/// A rig for a host of `platform` on `arch`, whose state directory is the platform's.
fn rig_on(name: &str, platform: Platform, arch: Arch) -> Rig {
    let dir = kbf_host::testkit::scratch(&format!("stage-{name}"));
    for sub in [
        "run",
        "var/lib/kbf-installer",
        "var/db/kbf-installer",
        "var/run",
        "usr/local/kbf-installer",
    ] {
        fs::create_dir_all(dir.join(sub)).expect("seed");
    }
    let host = Layout::new(Root::new(&dir).expect("absolute"), platform);
    let mut rig = Rig::on(host, dir);
    rig.settings.arch = arch;
    rig
}

fn facts_for(platform: &str) -> Facts {
    Facts {
        platform: platform.to_owned(),
        ..facts()
    }
}

/// The fetch step's hand-over of one tarball per `(name, binary)`, and the order's
/// locators for them, in the same order.
fn fetch_all(rig: &Rig, parts: &[(&str, Vec<u8>)]) -> (Vec<Locator>, Fetched) {
    let partial = Partial::create(&rig.host, &commit(), owner()).expect("partial");
    let (mut locators, mut files) = (Vec::new(), Vec::new());
    for (name, binary) in parts {
        let bytes = tar_gz(&[file(name, binary)]);
        let path = partial.dl().join(format!("{name}.tar.gz"));
        fs::write(&path, &bytes).expect("seed");
        locators.push(Locator {
            name: (*name).to_owned(),
            sha256: sha(&bytes),
            size: bytes.len() as u64,
            attestation: "{\"mediaType\":\"bundle\"}\n".to_owned(),
        });
        files.push(Received {
            name: (*name).to_owned(),
            tarball: path,
            bytes: bytes.len() as u64,
            sha256: sha(&bytes),
        });
    }
    (locators, Fetched { partial, files })
}

/// Which step of the second component (`kbf-daemon`) a scripted runner fails.
#[derive(Clone, Copy, PartialEq)]
enum Fails {
    Nothing,
    Gh,
    Quarantine,
    Signature,
    Probe,
}

/// Every program answered as an honest host would, except that `fails` goes wrong for
/// `kbf-daemon` alone: the first component is always fine.
fn scripted(fails: Fails) -> FakeRunner {
    FakeRunner::new(move |s| {
        let tool = s.program.file_name().expect("name").to_string_lossy();
        let daemon = s.program.ends_with("kbf-daemon")
            || s.args
                .iter()
                .any(|a| a.to_string_lossy().contains("kbf-daemon"));
        let bad = |step| daemon && fails == step;
        Ok(match &*tool {
            "gh" if bad(Fails::Gh) => ran(1, "", "no matching attestation\n"),
            "xattr" if bad(Fails::Quarantine) => ran(0, "0081;5f;Safari;\n", ""),
            "xattr" => ran(1, "", "xattr: No such xattr: com.apple.quarantine\n"),
            "codesign" if s.args[0] == "--display" => ran(0, "", SIGNED),
            "codesign" if bad(Fails::Signature) => ran(1, "", "invalid signature\n"),
            "gh" | "codesign" => ran(0, "", ""),
            _ if bad(Fails::Probe) => ran(3, "", ""),
            _ => ran(0, &version_json(COMMIT, "release"), ""),
        })
    })
}

/// The pair of Linux components an `all-in-one` node runs.
fn linux_pair() -> [(&'static str, Vec<u8>); 2] {
    [
        ("kbf", elf(0x3e, "payload")),
        ("kbf-daemon", elf(0x3e, "daemon")),
    ]
}

fn activate_pair(
    rig: &Rig,
    parts: &[(&str, Vec<u8>)],
    platform: &str,
    runner: &FakeRunner,
) -> Result<Staged, Refusal> {
    let (locators, fetched) = fetch_all(rig, parts);
    rig.verify(
        runner,
        OrderKind::Activate,
        &locators,
        Some(&facts_for(platform)),
        fetched,
    )
}

/// Catches a rig that leaves a release behind: the scratch root of a test that installs
/// holds an executable (the fake ELF in `releases/<commit>/`), which a coverage run would
/// scan as an object and fail on. Dropping the rig removes the root, installed or not.
///
/// Defect seen red: the rig without a `Drop`.
#[test]
fn a_rig_takes_its_scratch_root_with_it() {
    let rig = Rig::new("drop");
    rig.activate(&honest()).expect("installed");
    let binary = rig.release().join("kbf");
    assert!(binary.exists(), "the release holds an executable");
    let dir = rig.dir.clone();
    drop(rig);
    assert!(!dir.exists(), "the root is gone");
}

/// Catches an order of several components judged by its first alone: every component is
/// handed to `gh` (each its own tarball), unpacked and run, and one bad component, the
/// second, ends the stage with nothing installed. The macOS set is two components.
///
/// Mutants seen red: the attestation loop over the first component only; the unpack and
/// probe loop over the first only.
#[test]
fn every_component_of_an_order_is_attested_and_probed() {
    let rig = Rig::new("pair-honest");
    let runner = scripted(Fails::Nothing);
    let staged = activate_pair(&rig, &linux_pair(), "linux-x86_64", &runner).expect("staged");
    assert_eq!(staged.components, ["kbf", "kbf-daemon"]);
    assert_eq!(programs(&runner), ["gh", "gh", "kbf", "kbf-daemon"]);
    let calls = runner.calls();
    for (call, tarball) in calls[..2].iter().zip(["kbf.tar.gz", "kbf-daemon.tar.gz"]) {
        assert!(
            call.args[2].to_string_lossy().ends_with(tarball),
            "{call:?}"
        );
    }
    for (probe, name) in calls[2..].iter().zip(["kbf", "kbf-daemon"]) {
        assert_eq!(probe.program.file_name().expect("name"), name);
        // The empty directory the stage made, not `/` or anything the binary could read.
        assert_eq!(probe.cwd.file_name().expect("cwd"), "empty");
    }
    for name in ["kbf", "kbf-daemon", "VERIFIED"] {
        assert!(rig.release().join(name).exists(), "{name}");
    }

    let rig = Rig::new("pair-gh");
    let runner = scripted(Fails::Gh);
    let err = activate_pair(&rig, &linux_pair(), "linux-x86_64", &runner).expect_err("gh");
    assert_eq!(err.word(), Some("attestation"), "{err}");
    assert_eq!(
        programs(&runner),
        ["gh", "gh", "gh"],
        "the first component passes; the second is asked once per accepted signer; nothing run"
    );
    rig.assert_nothing_installed();

    let rig = Rig::new("pair-probe");
    let runner = scripted(Fails::Probe);
    let err = activate_pair(&rig, &linux_pair(), "linux-x86_64", &runner).expect_err("probe");
    assert_eq!(err.word(), Some("facts"), "{err}");
    assert_eq!(programs(&runner), ["gh", "gh", "kbf", "kbf-daemon"]);
    rig.assert_nothing_installed();
}

/// Catches the platform of an arm64 Linux host spelled another way: the order says
/// `linux-arm64`, and the header the stage accepts is the ELF of machine 183.
///
/// Mutant seen red: the platform derived from the host alone (`linux-x86_64`).
#[test]
fn an_arm64_linux_release_is_linux_arm64() {
    let rig = rig_on("arm64", Platform::Linux, Arch::Aarch64);
    let parts = [("kbf", elf(0xb7, "payload"))];
    let staged =
        activate_pair(&rig, &parts, "linux-arm64", &scripted(Fails::Nothing)).expect("staged");
    assert_eq!(staged.platform, "linux-arm64");

    let rig = rig_on("arm64-wrong", Platform::Linux, Arch::Aarch64);
    let err = activate_pair(&rig, &parts, "linux-x86_64", &scripted(Fails::Nothing))
        .expect_err("another platform than the order's");
    assert_eq!(err.word(), Some("facts"), "{err}");
}

fn mac_pair() -> [(&'static str, Vec<u8>); 2] {
    [
        ("kbf", macho(ARM64_MACHO, "payload")),
        ("kbf-daemon", macho(ARM64_MACHO, "daemon")),
    ]
}

/// Catches the macOS checks being skipped or applied to one component: each Mach-O is
/// checked for the quarantine attribute and a signature before it is run, in that order,
/// the release is `darwin-arm64`, and a quarantined or badly signed second component ends
/// the stage before its binary runs.
///
/// Mutants seen red: the quarantine check dropped; the signature check dropped; either
/// applied to the first component only.
#[test]
fn a_macos_release_is_checked_for_quarantine_and_signature_per_component() {
    let rig = rig_on("mac", Platform::MacOs, Arch::Aarch64);
    let runner = scripted(Fails::Nothing);
    let staged = activate_pair(&rig, &mac_pair(), "darwin-arm64", &runner).expect("staged");
    assert_eq!(staged.platform, "darwin-arm64");
    assert_eq!(
        programs(&runner),
        [
            "gh",
            "gh",
            "xattr",
            "codesign",
            "codesign",
            "kbf",
            "xattr",
            "codesign",
            "codesign",
            "kbf-daemon"
        ]
    );
    assert!(rig.release().join("kbf-daemon").exists());
    assert!(
        rig.host.join(&rig.host.state_file()).exists(),
        "macOS state dir"
    );

    for (case, fails, code) in [
        ("quarantine", Fails::Quarantine, "quarantined"),
        ("signature", Fails::Signature, "signature-invalid"),
    ] {
        let rig = rig_on(&format!("mac-{case}"), Platform::MacOs, Arch::Aarch64);
        let runner = scripted(fails);
        let err = activate_pair(&rig, &mac_pair(), "darwin-arm64", &runner).expect_err(case);
        assert_eq!(err.word(), Some("shape"), "{case}: {err}");
        assert_eq!(err.code(), code, "{case}");
        assert!(
            !programs(&runner).contains(&"kbf-daemon".to_owned()),
            "{case}: the second binary was not run"
        );
        rig.assert_nothing_installed();
    }
}

/// Catches a stage that cannot tell whether a release is already there going on: a
/// `releases/<commit>` that cannot be examined (its directory is closed) ends the stage
/// with the OS error and nothing installed. This covers the examination's error arm; a
/// mutant that treats the error as "not there" is equivalent here (the rename into the
/// closed directory fails too), so none is claimed.
#[test]
fn a_release_directory_that_cannot_be_examined_is_an_error() {
    if me().uid == 0 {
        return; // root reads a closed directory
    }
    let rig = Rig::new("unexaminable");
    let releases = rig.host.join(&rig.host.releases());
    fs::create_dir_all(&releases).expect("releases");
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o000)).expect("close");
    let err = rig.activate(&honest());
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
    let err = err.expect_err("cannot examine");
    assert!(matches!(err, Refusal::Work { .. }), "{err}");
    rig.assert_nothing_installed();
}

/// Catches an already-installed release returning before the state names it: the same
/// release found in place is reported as such, and `staged` names it all the same (a crash
/// between the rename and the state write leaves exactly this).
///
/// Mutant seen red: the early return before `staged` is set.
#[test]
fn an_installed_release_found_again_is_staged_again() {
    let rig = Rig::new("again");
    rig.activate(&honest()).expect("first");
    {
        let lock = Locked::acquire(&rig.host).expect("lock");
        lock.write(&State::default()).expect("forget the stage");
    }
    let staged = rig.activate(&honest()).expect("second");
    assert!(staged.already_installed);
    let state = crate::state::read_unlocked(&rig.host)
        .expect("state")
        .expect("written");
    assert_eq!(state.staged, Some(commit()));
}

/// Catches a binary that cannot be started being read as a clean report: the stage ends with
/// the tool-failed code (a failure of the host, so it carries no refusal word), says what
/// could not run, and nothing is installed.
///
/// Mutant planted: the start error mapped to `Ok` with an empty report.
#[test]
fn a_binary_that_cannot_be_started_is_a_facts_refusal() {
    let rig = Rig::new("probe-cannot-start");
    let runner = FakeRunner::new(|s| {
        if s.program.file_name().is_some_and(|n| n == "gh") {
            Ok(ran(0, "", ""))
        } else {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    });
    let err = rig.activate(&runner).expect_err("cannot start");
    assert_eq!(err.word(), None, "{err}");
    assert_eq!(err.code(), "tool-failed");
    assert!(err.to_string().contains("--version --json"), "{err}");
    rig.assert_nothing_installed();
}

/// Catches a component of size 0 being read (the order's cap check is `0 < size <= max`): it
/// is refused as `size` before a byte of the file is read or `gh` is run.
///
/// Mutant planted: the zero test dropped.
#[test]
fn a_component_of_size_zero_is_refused_before_it_is_read() {
    let rig = Rig::new("size-zero");
    let (mut locator, fetched) = rig.fetch(&tarball());
    locator.size = 0;
    let runner = honest();
    let err = rig
        .verify(
            &runner,
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("zero");
    assert_eq!(err.word(), Some("size"), "{err}");
    assert!(runner.calls().is_empty());
}

/// Catches an unusable state being worked around at the last step: with both state files
/// damaged the release has passed every check, and the stage still refuses (install-failed)
/// instead of recording a `staged` over a state it cannot read.
///
/// Mutant planted: the read error replaced by an empty state (the damaged file is overwritten).
#[test]
fn an_unusable_state_ends_the_install_after_the_checks() {
    let rig = Rig::new("state-unusable");
    for file in [rig.host.state_file(), rig.host.state_backup()] {
        fs::write(rig.host.join(&file), b"{ not json").expect("ruin");
    }
    let err = rig.activate(&honest()).expect_err("unusable");
    assert_eq!(err.code(), "install-failed", "{err}");
}

/// Catches the fetch child's own count of the stream being ignored: the file on disk hashes to
/// the order's digest, but the digest the parent computed while it read the stream from the
/// child does not, so the child handed over something other than it read; the stage ends as
/// `hash` and runs nothing.
///
/// Mutant planted: the parent's digest not compared with the order's (the on-disk hash alone
/// decides).
#[test]
fn a_stream_the_parent_hashed_differently_from_the_file_is_a_hash_refusal() {
    let rig = Rig::new("stream-digest");
    let (locator, mut fetched) = rig.fetch(&tarball());
    fetched.files[0].sha256 = "0".repeat(64);
    let runner = honest();
    let err = rig
        .verify(
            &runner,
            OrderKind::Activate,
            &[locator],
            Some(&facts()),
            fetched,
        )
        .expect_err("different digests");
    assert_eq!(err.word(), Some("hash"), "{err}");
    assert!(runner.calls().is_empty());
}
