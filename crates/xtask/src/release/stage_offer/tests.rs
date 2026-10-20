use kbf_testkit::dir::TestDir;

use super::*;

const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-stage-offer-unit"), name)
}

fn tarball(binary: &str, platform: &str) -> String {
    format!("{binary}-{SHA}-{platform}.tar.gz")
}

/// The names `SHA256SUMS` lists for `tarballs`, each with its SBOM; and the bundle of each of
/// `bundled` written to `dir`.
fn release_of(dir: &Path, tarballs: &[String], bundled: &[String]) -> Vec<String> {
    let mut listed = Vec::new();
    for t in tarballs {
        listed.push(t.clone());
        listed.push(format!("{}.cdx.json", t.trim_end_matches(".tar.gz")));
    }
    for t in bundled {
        std::fs::write(dir.join(bundles::name_of(t)), b"bundle").expect("bundle");
    }
    listed
}

/// What `farm_platforms` made of the release `tarballs`, all bundled: the complete
/// platforms' names and the skipped ones'.
fn served(name: &str, tarballs: &[String]) -> (Vec<String>, Vec<(String, String)>) {
    let dir = scratch(name);
    let listed = release_of(&dir, tarballs, tarballs);
    let (complete, skipped) = farm_platforms(&dir, &listed, SHA);
    (
        complete.iter().map(|p| p.to_string()).collect(),
        skipped
            .into_iter()
            .map(|(p, why)| (p.to_string(), why))
            .collect(),
    )
}

#[test]
fn a_host_is_a_platform_only_when_the_release_is_built_for_it() {
    // Catches: a machine name taken for another platform (aarch64 is arm64 on Linux, and
    // the Mac is arm64 alone), a host with no release (an Intel Mac, another system, another
    // machine) given a platform to run a binary of, and a refusal that does not say which
    // flag to give.
    let table = [
        ("Linux", "x86_64", Some(Platform::LinuxX86_64)),
        ("Linux", "aarch64", Some(Platform::LinuxArm64)),
        ("Linux", "arm64", Some(Platform::LinuxArm64)),
        ("Darwin", "arm64", Some(Platform::DarwinArm64)),
        ("Darwin", "x86_64", None),
        ("Linux", "riscv64", None),
        ("FreeBSD", "x86_64", None),
    ];
    for (os, machine, want) in table {
        match (platform_named(os, machine), want) {
            (Ok(got), Some(want)) => assert_eq!(got, want, "{os} {machine}"),
            (Err(Error::Usage(why)), None) => {
                assert!(why.contains("--runner-platform"), "{why}");
            }
            (got, want) => panic!("{os} {machine}: {got:?}, wanted {want:?}"),
        }
    }
}

#[test]
fn a_platform_is_complete_with_every_tarball_a_host_of_it_needs() {
    // Catches: a Mac that has a `kbf-daemon` left out, a Linux host that has `kbf` left out,
    // and the order of the platforms (the bundle's is this one).
    let all = vec![
        tarball("kbf", "linux-x86_64"),
        tarball("kbf", "linux-arm64"),
        tarball("kbf-daemon", "darwin-arm64"),
    ];
    let (complete, skipped) = served("complete", &all);
    assert_eq!(complete, ["linux-x86_64", "linux-arm64", "darwin-arm64"]);
    assert!(skipped.is_empty(), "{skipped:?}");
}

#[test]
fn a_mac_with_a_kbf_and_no_kbf_daemon_is_left_out_and_says_why() {
    // Catches: any tarball of the platform counting as complete: the release builds a `kbf`
    // for the Mac and no `kbf-daemon`, which the Mac needs (`ReleasePlatform::components`).
    let some = vec![
        tarball("kbf", "linux-x86_64"),
        tarball("kbf", "darwin-arm64"),
    ];
    let (complete, skipped) = served("mac-kbf", &some);
    assert_eq!(complete, ["linux-x86_64"]);
    let left_out: Vec<&str> = skipped.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(left_out, ["linux-arm64", "darwin-arm64"]);
    let (_, why) = &skipped[1];
    let want = format!("kbf-daemon-{SHA}-darwin-arm64.tar.gz is not in SHA256SUMS");
    assert!(why.contains(&want), "{why}");
    let (_, why) = &skipped[0];
    assert!(why.contains("no tarball"), "{why}");
}

#[test]
fn a_platform_without_its_sbom_or_its_bundle_is_left_out() {
    // Catches: the SBOM and the bundle not asked of a platform (`incomplete`'s checks must
    // reach the verb): the tarball is listed in both cases.
    let dir = scratch("sbom-bundle");
    let linux = vec![tarball("kbf", "linux-x86_64")];
    // Listed with its SBOM, bundle written: complete.
    let listed = release_of(&dir, &linux, &linux);
    let (complete, _) = farm_platforms(&dir, &listed, SHA);
    assert_eq!(complete, [ReleasePlatform::LinuxX86_64]);
    // No SBOM listed.
    let no_sbom = vec![tarball("kbf", "linux-x86_64")];
    let (complete, skipped) = farm_platforms(&dir, &no_sbom, SHA);
    assert!(complete.is_empty(), "{complete:?}");
    assert!(skipped[0].1.contains("has no SBOM"), "{skipped:?}");
    // No bundle on disk.
    let other = scratch("no-bundle");
    let listed = release_of(&other, &linux, &[]);
    let (complete, skipped) = farm_platforms(&other, &listed, SHA);
    assert!(complete.is_empty(), "{complete:?}");
    assert!(skipped[0].1.contains("has no bundle"), "{skipped:?}");
}

#[test]
fn the_offers_own_attestation_check_is_a_check_already_made() {
    // Catches: the stand-in asking a question (or refusing): the release is verified once,
    // before anything runs, and the offer must not ask GitHub for every file again.
    Verified.artifacts(Path::new("dir"), SHA).expect("no question");
    Verified.beta(Path::new("result"), SHA).expect("no question");
}

#[test]
fn the_trusted_root_is_what_gh_printed_and_a_gh_that_cannot_run_is_an_error() {
    // Catches: a root that is not written where the verification reads it, a write that
    // fails ignored, and a gh that does not start taken for an empty root. `true` is a gh
    // that prints nothing and succeeds.
    let dir = scratch("root");
    let path = trusted_root(Path::new("true"), &dir).expect("a root");
    assert_eq!(path, dir.join("trusted_root.jsonl"));
    assert!(std::fs::read(&path).expect("the root").is_empty());
    let gone = dir.join("gone");
    let made = trusted_root(Path::new("true"), &gone);
    let why = made.expect_err("no directory").to_string();
    assert!(why.contains("write "), "{why}");
    let made = trusted_root(Path::new("no-such-gh-program"), &dir);
    let why = made.expect_err("no program").to_string();
    assert!(why.contains("no-such-gh-program"), "{why}");
}
