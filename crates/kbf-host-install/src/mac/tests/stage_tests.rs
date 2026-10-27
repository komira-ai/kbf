//! What is checked before anything on the Mac changes: each check, in its order, on the
//! one copy in root's stage.

use std::cell::RefCell;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::rc::Rc;

use super::fx::{Fx, NEW, NEW_VERSION, Sim, digest, gzip, mkdir, mode, sha256, tarball, write};
use crate::mac::channel::{MacChannel, Source};
use crate::mac::release::{Origin, bundle_name, tarball_name};
use crate::mac::stage::{Staged, stage, strip_quarantine};
use crate::outcome::{FailureCode, Outcome, Refusal};
use crate::request::Version;
use crate::runner::Identity;

fn channel(fx: &Fx) -> MacChannel {
    MacChannel {
        source: Source::Dir(fx.channel_dir.clone()),
        sums: Some(fx.sums.clone()),
        source_ref: "refs/heads/main".into(),
        origin: Origin::kbf(),
    }
}

/// The channel with no sums: the attestation alone decides.
fn unpinned(fx: &Fx) -> MacChannel {
    MacChannel {
        sums: None,
        ..channel(fx)
    }
}

fn run_unpinned(fx: &Fx, sim: &Sim, version: &str) -> Result<Staged, Outcome> {
    mkdir(&fx.host.work, 0o711);
    stage(
        &fx.host,
        sim,
        &unpinned(fx),
        &Version::new(version).unwrap(),
    )
}

fn run(fx: &Fx, sim: &Sim, version: &str) -> Result<Staged, Outcome> {
    mkdir(&fx.host.work, 0o711);
    stage(&fx.host, sim, &channel(fx), &Version::new(version).unwrap())
}

fn failure(result: Result<Staged, Outcome>) -> (FailureCode, String) {
    match result {
        Err(Outcome::Failed(f)) => (f.code, f.detail),
        other => panic!("not a failure: {other:?}"),
    }
}

/// The detail of a check that failed; the stage is gone.
fn verify_failure(fx: &Fx, sim: &Sim) -> String {
    let (code, detail) = failure(run(fx, sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed, "{detail}");
    assert!(!fx.host.stage().exists(), "a failed check leaves no stage");
    detail
}

fn tar_of(fx: &Fx, members: &[(&str, tar::EntryType, &[u8])]) {
    fx.offer(&tarball(members));
}

/// A short name for each tool a call ran.
fn tags(fx: &Fx, sim: &Sim) -> Vec<&'static str> {
    sim.calls
        .borrow()
        .iter()
        .map(|c| {
            if c.program == fx.host.gh.to_str().unwrap() {
                "gh"
            } else if c.program.ends_with("/kbf") {
                "check"
            } else if c.program.ends_with("dscl") {
                "dscl"
            } else if c.program.ends_with("xattr") {
                "xattr"
            } else {
                "codesign"
            }
        })
        .collect()
}

/// A good release is staged: the one copy is in the stage, root's and sealed 0700, the
/// answer names the commit and the hash of the binary, and nothing outside the work
/// directory changed.
#[test]
fn a_good_release_is_staged() {
    let fx = Fx::new("mac-stage-ok");
    let sim = fx.sim();
    let staged = run(&fx, &sim, NEW_VERSION).unwrap();
    assert_eq!(
        staged,
        Staged {
            commit: NEW.into(),
            binary_sha256: sha256(&fx.new_binary)
        }
    );
    let stage = fx.host.stage();
    assert_eq!(mode(&stage), 0o700);
    assert_eq!(fs::read(stage.join("kbf")).unwrap(), fx.new_binary);
    assert_eq!(mode(&stage.join("kbf")), 0o755);
    assert_eq!(
        fs::read(stage.join(tarball_name(NEW))).unwrap(),
        fs::read(fx.tarball_path()).unwrap()
    );
    assert_eq!(
        fs::read(stage.join(bundle_name(NEW))).unwrap(),
        fs::read(fx.bundle_path()).unwrap()
    );
    assert_eq!(digest(&stage.join("kbf")), staged.binary_sha256);
    assert_eq!(
        fx.link(&fx.host.current()).as_deref(),
        Some(format!("releases/{}", super::fx::OLD).as_str())
    );
}

/// The checks run in order, on the stage's copy and never on the drop directory's file; the
/// two commands that run as another user run with the stage open to traverse (0711) and as
/// the right account, and every other step with it sealed (0700).
#[test]
fn the_checks_run_on_the_copy_in_order() {
    let fx = Fx::new("mac-stage-order");
    let sim = fx.sim();
    let stage_dir = fx.host.stage();
    let seen = Rc::new(RefCell::new(Vec::new()));
    {
        let (seen, stage_dir) = (seen.clone(), stage_dir.clone());
        *sim.before.borrow_mut() = Some(Box::new(move |_| {
            let now = fs::metadata(&stage_dir).map_or(0, |m| m.mode() & 0o777);
            seen.borrow_mut().push(now);
        }));
    }
    run(&fx, &sim, NEW_VERSION).unwrap();
    assert_eq!(
        tags(&fx, &sim),
        [
            "dscl", "dscl", "gh", "xattr", "codesign", "codesign", "check"
        ]
    );
    assert_eq!(*seen.borrow(), [0, 0, 0o711, 0o700, 0o700, 0o700, 0o711]);
    assert_eq!(mode(&stage_dir), 0o700);
    let calls = sim.calls.borrow();
    let meta = fs::metadata(&*fx.dir).unwrap();
    let me = Identity {
        uid: meta.uid(),
        gid: meta.gid(),
    };
    assert_eq!(calls[2].run_as, Some(me));
    assert_eq!(calls[6].run_as, Some(me));
    let gh_args = calls[2].args.join(" ");
    assert!(gh_args.contains(&stage_dir.join(tarball_name(NEW)).display().to_string()));
    assert!(gh_args.contains(&stage_dir.join(bundle_name(NEW)).display().to_string()));
    assert!(!gh_args.contains(&fx.channel_dir.display().to_string()));
    assert!(gh_args.contains(&format!("--source-digest {NEW}")));
    assert_eq!(
        calls[6].program,
        stage_dir.join("kbf").display().to_string()
    );
    assert_eq!(
        calls[6].args[2],
        fx.host.daemon_config.display().to_string()
    );
    // The quarantine, signature and runtime checks name the staged binary.
    for call in &calls[3..6] {
        assert!(
            call.args.last().unwrap().ends_with("/stage/kbf"),
            "{call:?}"
        );
    }
}

/// The request's version must carry a commit, and the sums must list a release of it.
#[test]
fn a_version_that_names_no_listed_release_is_not_staged() {
    let fx = Fx::new("mac-stage-version");
    let sim = fx.sim();
    let (code, detail) = failure(run(&fx, &sim, "1.2.3-1"));
    assert_eq!(
        (code, detail.contains("carries no commit")),
        (FailureCode::VerifyFailed, true)
    );
    let (code, detail) = failure(run(&fx, &sim, "0.1.7+gcccccccccccc-3"));
    assert_eq!(
        (code, detail.contains("unknown release")),
        (FailureCode::VerifyFailed, true)
    );
    // Two releases of the sums start with the digits.
    let twin = format!("{}c", &NEW[..39]);
    let sums = fs::read_to_string(&fx.sums).unwrap();
    write(
        &fx.sums,
        0o644,
        format!("{sums}{}  {}\n", "9".repeat(64), tarball_name(&twin)).as_bytes(),
    );
    let (code, detail) = failure(run(&fx, &sim, NEW_VERSION));
    assert_eq!(
        (code, detail.contains("ambiguous release")),
        (FailureCode::VerifyFailed, true)
    );
    assert_eq!(sim.calls.borrow().len(), 0, "nothing ran");
    assert!(!fx.host.stage().exists());
}

/// Sums that are not plain lines are refused as configuration; sums others can write are
/// unsafe; pinned tools others can write are untrusted.
#[test]
fn unusable_pins_are_refused() {
    let fx = Fx::new("mac-stage-pins");
    let sim = fx.sim();
    write(&fx.sums, 0o644, b"not sums");
    assert!(matches!(
        run(&fx, &sim, NEW_VERSION),
        Err(Outcome::Refused(Refusal::ConfigInvalid))
    ));
    write(&fx.sums, 0o666, b"x");
    assert!(matches!(
        run(&fx, &sim, NEW_VERSION),
        Err(Outcome::Refused(Refusal::ConfigUnsafe))
    ));
    let fx = Fx::new("mac-stage-pins-gh");
    let sim = fx.sim();
    fs::set_permissions(&fx.host.gh, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        run(&fx, &sim, NEW_VERSION),
        Err(Outcome::Refused(Refusal::KeyUntrusted))
    ));
    fs::set_permissions(&fx.host.gh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(&fx.host.trusted_root).unwrap();
    assert!(matches!(
        run(&fx, &sim, NEW_VERSION),
        Err(Outcome::Refused(Refusal::ConfigInvalid))
    ));
    assert_eq!(sim.calls.borrow().len(), 0);
}

/// An account the checks run as must exist with ids that are not root's.
#[test]
fn the_accounts_must_exist_and_not_be_root() {
    let fx = Fx::new("mac-stage-accounts");
    let sim = fx.sim();
    sim.break_("dscl");
    assert!(verify_failure_of(&fx, &sim).contains("dscl: exit status"));
    let sim = Sim::new(&fx.host, 0, 0);
    assert!(verify_failure_of(&fx, &sim).contains("no usable ids"));
    // A real uid with group 0 (`wheel`) is refused as well.
    let sim = Sim::new(&fx.host, 301, 0);
    assert!(verify_failure_of(&fx, &sim).contains("no usable ids"));
    let sim = fx.sim();
    sim.break_("launchctl"); // not used here: the accounts are asked with dscl
    run(&fx, &sim, NEW_VERSION).unwrap();
}

fn verify_failure_of(fx: &Fx, sim: &Sim) -> String {
    let (code, detail) = failure(run(fx, sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed, "{detail}");
    detail
}

/// The files of the drop directory are untrusted: a missing file, a link, a file with two
/// names, one past the limit, are each refused, and nothing is run.
#[test]
fn the_drop_directory_files_are_taken_with_care() {
    let fx = Fx::new("mac-stage-copy");
    let sim = fx.sim();
    let tar = fx.tarball_path();
    let bundle = fx.bundle_path();
    let good = fs::read(&tar).unwrap();

    fs::remove_file(&tar).unwrap();
    let detail = verify_failure(&fx, &sim);
    assert!(
        detail.starts_with("tarball:") && detail.contains("No such file"),
        "{detail}"
    );
    fs::write(fx.dir.join("elsewhere"), &good).unwrap();
    symlink(fx.dir.join("elsewhere"), &tar).unwrap();
    let detail = verify_failure(&fx, &sim);
    assert!(
        detail.starts_with("tarball:") && detail.contains("symbolic links"),
        "a link: {detail}"
    );
    fs::remove_file(&tar).unwrap();
    fs::hard_link(fx.dir.join("elsewhere"), &tar).unwrap();
    assert!(verify_failure(&fx, &sim).contains("one link"));
    fs::remove_file(&tar).unwrap();
    fs::remove_file(fx.dir.join("elsewhere")).unwrap();
    mkdir(&tar, 0o755);
    assert!(
        verify_failure(&fx, &sim).contains("one link"),
        "a directory"
    );
    fs::remove_dir(&tar).unwrap();
    write(&tar, 0o644, &vec![0_u8; 64 * 1024 + 1]);
    assert!(verify_failure(&fx, &sim).contains("larger than"));
    write(&tar, 0o644, &good);

    fs::remove_file(&bundle).unwrap();
    let detail = verify_failure(&fx, &sim);
    assert!(
        detail.starts_with("attestation bundle:") && detail.contains("No such file"),
        "{detail}"
    );
    write(&bundle, 0o644, &vec![b'x'; 4 * 1024 + 1]);
    assert!(verify_failure(&fx, &sim).contains("attestation bundle: larger than"));
    assert!(
        sim.calls
            .borrow()
            .iter()
            .all(|c| c.program.ends_with("dscl")),
        "no check ran"
    );
}

/// A stage that cannot be made is a failure.
#[test]
fn a_stage_that_cannot_be_made_is_a_failure() {
    let fx = Fx::new("mac-stage-nomake");
    let sim = fx.sim();
    // No work directory at all.
    let (code, detail) = failure(stage(
        &fx.host,
        &sim,
        &channel(&fx),
        &Version::new(NEW_VERSION).unwrap(),
    ));
    assert_eq!(code, FailureCode::InstallFailed, "{detail}");
    assert!(detail.starts_with("stage:"));
    // A file where the stage must go.
    mkdir(&fx.host.work, 0o711);
    write(&fx.host.stage(), 0o600, b"");
    let (code, _) = failure(run(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::InstallFailed);
    // A stage left by an earlier run is replaced.
    fs::remove_file(fx.host.stage()).unwrap();
    mkdir(&fx.host.stage(), 0o700);
    write(&fx.host.stage().join("old"), 0o600, b"left over");
    run(&fx, &sim, NEW_VERSION).unwrap();
    assert!(!fx.host.stage().join("old").exists());
}

/// The sha256 of the copy must be the pinned one: a changed tarball is refused before any
/// tool runs on it.
#[test]
fn a_tarball_that_is_not_the_pinned_one_is_refused_before_anything_runs() {
    let fx = Fx::new("mac-stage-sha");
    let sim = fx.sim();
    let mut tar = fs::read(fx.tarball_path()).unwrap();
    let last = tar.len() - 1;
    tar[last] ^= 1;
    write(&fx.tarball_path(), 0o644, &tar);
    assert!(verify_failure(&fx, &sim).contains("not the pinned one"));
    assert_eq!(
        tags(&fx, &sim),
        ["dscl", "dscl"],
        "no attestation was asked"
    );
}

/// The attestation is accepted under either name of the repository, and refused when
/// neither accepts.
#[test]
fn the_attestation_must_verify() {
    let fx = Fx::new("mac-stage-attest");
    let sim = fx.sim();
    sim.break_("gh");
    let detail = verify_failure(&fx, &sim);
    assert!(
        detail.contains("not attested") && detail.contains("no matching attestations"),
        "{detail}"
    );
    assert_eq!(
        tags(&fx, &sim),
        ["dscl", "dscl", "gh", "gh"],
        "both signers were tried"
    );
    let sim = fx.sim();
    sim.break_("gh-current");
    run(&fx, &sim, NEW_VERSION).unwrap();
    assert_eq!(tags(&fx, &sim).iter().filter(|t| **t == "gh").count(), 2);
    let args = sim.calls.borrow()[3].args.join(" ");
    assert!(args.contains(
        "--repo komira-ai/komira-build-farm --signer-workflow komira-ai/komira-build-farm/"
    ));
}

/// The tarball must hold exactly one regular file named `kbf`, and it must be an arm64
/// Mach-O file.
#[test]
fn the_tarball_holds_exactly_the_binary() {
    use tar::EntryType::{Directory, Regular};
    let fx = Fx::new("mac-stage-tar");
    let sim = fx.sim();
    let bin = fx.new_binary.clone();
    for (why, members, want) in [
        (
            "another name",
            vec![("kbf2", Regular, &bin[..])],
            "not named kbf",
        ),
        (
            "a path",
            vec![("bin/kbf", Regular, &bin[..])],
            "not named kbf",
        ),
        (
            "a directory",
            vec![("kbf", Directory, &b""[..])],
            "not a regular file",
        ),
        (
            "two members",
            vec![("kbf", Regular, &bin[..]), ("x", Regular, &b"x"[..])],
            "more than one",
        ),
        (
            "too large",
            vec![("kbf", Regular, &vec![0xcf_u8; 16 * 1024 + 1][..])],
            "larger than",
        ),
        (
            "not Mach-O",
            vec![("kbf", Regular, &b"plain text, not a program\n"[..])],
            "not an arm64 Mach-O",
        ),
        (
            "32-bit Mach-O",
            vec![("kbf", Regular, &[0xce_u8, 0xfa, 0xed, 0xfe, 7, 0, 0, 1][..])],
            "not an arm64 Mach-O",
        ),
        (
            "short",
            vec![("kbf", Regular, &b"\xcf\xfa"[..])],
            "failed to fill",
        ),
    ] {
        tar_of(&fx, &members);
        let detail = verify_failure(&fx, &sim);
        assert!(detail.contains(want), "{why}: {detail}");
    }
    // Not gzip, and nothing at all.
    fx.offer(b"plain text, not a tarball");
    assert!(verify_failure(&fx, &sim).starts_with("tarball:"));
    fx.offer(&gzip(b""));
    assert!(verify_failure(&fx, &sim).contains("holds no file"));
    // Attestation had passed each time: the checks after it are the ones that spoke.
    assert!(tags(&fx, &sim).contains(&"gh"));
}

/// A quarantine attribute on the binary is removed and proven gone; one that stays, or a
/// tool that fails, is a refusal.
#[test]
fn the_quarantine_attribute_is_removed_and_proven_gone() {
    let fx = Fx::new("mac-stage-quarantine");
    let sim = fx.sim();
    let binary = fx.host.stage().join("kbf");
    sim.quarantined.borrow_mut().insert(binary.clone());
    run(&fx, &sim, NEW_VERSION).unwrap();
    assert!(sim.quarantined.borrow().is_empty());
    assert_eq!(sim.count("/usr/bin/xattr -d com.apple.quarantine"), 1);
    assert_eq!(sim.count("/usr/bin/xattr /"), 2, "listed before and after");

    for (why, broken, want) in [
        ("it stays", "xattr-sticky", "stays on kbf"),
        ("cannot remove", "xattr-d", "xattr -d"),
        ("cannot list", "xattr", "xattr: exit status"),
    ] {
        let sim = fx.sim();
        sim.quarantined.borrow_mut().insert(binary.clone());
        sim.break_(broken);
        let detail = verify_failure(&fx, &sim);
        assert!(detail.contains(want), "{why}: {detail}");
    }
    // Direct use: a file without the attribute is left alone.
    let sim = fx.sim();
    strip_quarantine(&fx.host, &sim, &binary).unwrap();
    assert_eq!(sim.count("/usr/bin/xattr -d"), 0);
}

/// The signature must verify and have the hardened runtime.
#[test]
fn the_signature_must_be_valid_and_hardened() {
    let fx = Fx::new("mac-stage-codesign");
    let sim = fx.sim();
    sim.break_("codesign-verify");
    assert!(verify_failure(&fx, &sim).contains("codesign --verify: exit status"));
    let sim = fx.sim();
    sim.break_("no-runtime");
    assert!(verify_failure(&fx, &sim).contains("no hardened runtime"));
}

/// The new binary must accept the live configuration as the daemon's user; its words are
/// the refusal's detail.
#[test]
fn the_new_binary_must_accept_the_live_configuration() {
    let fx = Fx::new("mac-stage-check");
    let sim = fx.sim();
    sim.break_("check");
    let detail = verify_failure(&fx, &sim);
    assert!(
        detail.contains("kbf --check: exit status Some(2): kbf: bad config"),
        "{detail}"
    );
    assert_eq!(tags(&fx, &sim).last(), Some(&"check"));
}

/// The staged binary must be root's in a tree only root can write.
#[test]
fn the_staged_binary_must_be_in_roots_tree() {
    let fx = Fx::new("mac-stage-owner");
    let sim = fx.sim();
    // A work directory others can write: the stage is not safe.
    mkdir(&fx.host.work, 0o777);
    let result = stage(
        &fx.host,
        &sim,
        &channel(&fx),
        &Version::new(NEW_VERSION).unwrap(),
    );
    let (code, detail) = failure(result);
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(detail.contains("in the stage"), "{detail}");
}

/// A stage that cannot be set up for the unprivileged account (its home cannot be given to
/// it, or the stage is gone when it is sealed) is an install failure, not a pass.
#[test]
fn a_stage_that_cannot_be_prepared_is_an_install_failure() {
    let fx = Fx::new("mac-stage-prepare");
    let sim = fx.sim();
    let meta = fs::metadata(&*fx.dir).unwrap();
    if meta.uid() != 0 {
        // The account's home is given to it with chown, which only root may give away.
        sim.nobody_ids.set(Some((meta.uid() + 1, meta.gid())));
        let (code, detail) = failure(run(&fx, &sim, NEW_VERSION));
        assert_eq!(code, FailureCode::InstallFailed, "{detail}");
        assert!(detail.starts_with("stage:"));
    }
    let sim = fx.sim();
    let stage_dir = fx.host.stage();
    *sim.before.borrow_mut() = Some(Box::new(move |cmd| {
        if cmd.program.ends_with("/gh") {
            let _ = fs::remove_dir_all(&stage_dir);
        }
    }));
    let (code, detail) = failure(run(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::InstallFailed, "{detail}");
}

/// A channel with no sums takes the release the attestation accepts, whatever its tarball
/// hashes to: the sums are not read. `gh` is asked to bind the full 40-digit commit as the
/// source digest, the channel's branch as the source ref, this repository's workflow as the
/// signer on hosted runners only, and to hash the staged tarball (its sha256 must be a
/// subject), all on the stage's copy.
#[test]
fn an_unpinned_channel_takes_what_the_attestation_accepts() {
    let fx = Fx::new("mac-stage-unpinned");
    let sim = fx.sim();
    // Sums that would refuse this tarball are not consulted.
    write(&fx.sums, 0o644, b"not sums at all");
    let staged = run_unpinned(&fx, &sim, NEW_VERSION).unwrap();
    assert_eq!(staged.commit, NEW);
    assert_eq!(staged.binary_sha256, sha256(&fx.new_binary));
    let calls = sim.calls.borrow();
    let gh = calls
        .iter()
        .find(|c| c.program == fx.host.gh.to_str().unwrap())
        .unwrap();
    let stage_dir = fx.host.stage();
    let args = &gh.args;
    let value_of = |flag: &str| {
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1].clone()
    };
    assert_eq!(args[..2], ["attestation", "verify"]);
    assert_eq!(
        args[2],
        stage_dir.join(tarball_name(NEW)).display().to_string()
    );
    assert_eq!(value_of("--source-digest"), NEW);
    assert_eq!(
        NEW.len(),
        40,
        "the whole commit, not the version's 12 digits"
    );
    assert_eq!(value_of("--source-ref"), "refs/heads/main");
    assert_eq!(value_of("--repo"), "komira-ai/kbf");
    assert_eq!(
        value_of("--signer-workflow"),
        "komira-ai/kbf/.github/workflows/artifacts.yml"
    );
    assert_eq!(value_of("--digest-alg"), "sha256");
    assert_eq!(
        value_of("--cert-oidc-issuer"),
        "https://token.actions.githubusercontent.com"
    );
    assert!(args.iter().any(|a| a == "--deny-self-hosted-runners"));
}

/// The branch the attestation must have been made for is the channel's own.
#[test]
fn the_attestation_is_bound_to_the_channels_branch() {
    let fx = Fx::new("mac-stage-ref");
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o711);
    let release = MacChannel {
        source_ref: "refs/heads/release/1".into(),
        ..unpinned(&fx)
    };
    stage(
        &fx.host,
        &sim,
        &release,
        &Version::new(NEW_VERSION).unwrap(),
    )
    .unwrap();
    let args = sim.calls.borrow()[2].args.join(" ");
    assert!(args.contains("--source-ref refs/heads/release/1"), "{args}");
}

/// With no sums, an attestation that does not verify refuses the release, and says it was
/// the attestation; nothing past it ran.
#[test]
fn an_unpinned_release_the_attestation_refuses_is_refused() {
    let fx = Fx::new("mac-stage-unattested");
    let sim = fx.sim();
    sim.break_("gh");
    let (code, detail) = failure(run_unpinned(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(
        detail.starts_with("not attested: gh attestation verify"),
        "{detail}"
    );
    assert_eq!(tags(&fx, &sim), ["dscl", "dscl", "gh", "gh"]);
    assert!(!fx.host.stage().exists());
}

/// With no sums, a version picks its tarball from the directory by the 12 digits: none is an
/// unknown release, two are ambiguous, and neither starts anything.
#[test]
fn an_unpinned_version_picks_its_tarball_from_the_directory() {
    let fx = Fx::new("mac-stage-listing");
    let sim = fx.sim();
    let (code, detail) = failure(run_unpinned(&fx, &sim, "0.1.7+gcccccccccccc-3"));
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(detail.starts_with("unknown release"), "{detail}");
    let twin = format!("{}c", &NEW[..39]);
    write(&fx.channel_dir.join(tarball_name(&twin)), 0o644, b"x");
    let (code, detail) = failure(run_unpinned(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(detail.starts_with("ambiguous release"), "{detail}");
    assert_eq!(sim.calls.borrow().len(), 0, "nothing ran");
    // A name that is not text and a file of another kind are not releases.
    fs::remove_file(fx.channel_dir.join(tarball_name(&twin))).unwrap();
    let odd = std::ffi::OsStr::from_bytes(b"kbf-\xff-darwin-arm64.tar.gz");
    write(&fx.channel_dir.join(odd), 0o644, b"x");
    assert_eq!(run_unpinned(&fx, &sim, NEW_VERSION).unwrap().commit, NEW);
}

/// A directory that cannot be listed is a failure of the check, not of the host.
#[test]
fn an_unpinned_channel_whose_directory_cannot_be_listed_fails() {
    let fx = Fx::new("mac-stage-nolisting");
    let sim = fx.sim();
    fs::remove_dir_all(&fx.channel_dir).unwrap();
    let (code, detail) = failure(run_unpinned(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(detail.starts_with("the channel's directory"), "{detail}");
}

/// Sums that are set are a freeze list on top of the attestation: a release the sums do not
/// list is refused although its attestation would pass, and listing it lets it through.
#[test]
fn pinned_sums_are_a_freeze_list() {
    let fx = Fx::new("mac-stage-freeze");
    let sim = fx.sim();
    let kept = format!(
        "{}  {}\n",
        sha256(b"another release"),
        tarball_name(super::fx::OLD)
    );
    write(&fx.sums, 0o644, kept.as_bytes());
    let (code, detail) = failure(run(&fx, &sim, NEW_VERSION));
    assert_eq!(code, FailureCode::VerifyFailed);
    assert!(detail.starts_with("unknown release"), "{detail}");
    assert_eq!(sim.calls.borrow().len(), 0, "nothing ran");
    let tar = fs::read(fx.tarball_path()).unwrap();
    write(
        &fx.sums,
        0o644,
        format!("{kept}{}  {}\n", sha256(&tar), tarball_name(NEW)).as_bytes(),
    );
    assert_eq!(run(&fx, &sim, NEW_VERSION).unwrap().commit, NEW);
}

/// An older release stages like a newer one, with or without sums.
#[test]
fn an_older_release_stages_with_and_without_sums() {
    let fx = Fx::new("mac-stage-older");
    let old = super::fx::OLD;
    fx.offer_release(old, &super::fx::binary("old release"));
    for pinned in [true, false] {
        let sim = fx.sim();
        let staged = if pinned {
            run(&fx, &sim, super::fx::OLD_VERSION)
        } else {
            run_unpinned(&fx, &sim, super::fx::OLD_VERSION)
        }
        .unwrap();
        assert_eq!(staged.commit, old);
        assert_eq!(
            staged.binary_sha256,
            sha256(&super::fx::binary("old release"))
        );
    }
}
