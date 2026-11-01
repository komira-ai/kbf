//! `cargo xtask build-commit check-asset` run as the artifacts workflow runs it: on the
//! tarball `package` wrote, with `xtask-fake` as the `kbf` inside it (its stamp and
//! identity ride on its own bytes, so the tarball decides what the extracted binary says).

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OTHER: &str = "d0ffee0123456789c0ffee0123456789c0ffee01";
const PLATFORM: &str = "linux-x86_64";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-check-asset"),
        name,
    )
}

/// The identity `kbf --version --json` prints, as a build of `commit` makes it.
fn identity(commit: &str, profile: &str) -> String {
    format!(
        "{{\"admin_protocol\":1,\"commit\":\"{commit}\",\"meta_log\":4,\"min_protocol\":1,\"profile\":\"{profile}\",\"protocol\":1,\"seq\":812,\"state_schema\":1,\"version\":\"0.1.0\"}}\n"
    )
}

/// The fake with the trailer it reads as `kbf`: `--version` names `stamp` and `--version
/// --json` prints `json`.
fn kbf_bytes(stamp: &str, json: &str) -> Vec<u8> {
    let mut bytes = std::fs::read(FAKE).expect("the fake");
    bytes.extend_from_slice(format!("\nxtask-fake-kbf\n{stamp}\n{json}").as_bytes());
    bytes
}

/// `kbf_bytes` of a release build of `commit`: what a correct build ships.
fn good_kbf(commit: &str) -> Vec<u8> {
    kbf_bytes(&commit[..12], &identity(&commit[..12], "release"))
}

/// `bytes` packaged as `name` for `commit` by `cargo xtask package`, as the workflow
/// runs it: the tarball's path.
fn packaged(root: &Path, name: &str, bytes: &[u8], commit: &str) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let binary = bin.join(name);
    std::fs::write(&binary, bytes).expect("binary");
    let sbom = root.join("sbom.cdx.json");
    std::fs::write(&sbom, "{}").expect("sbom");
    let dist = root.join("dist");
    let out = Command::new(XTASK)
        .arg("package")
        .arg("--binary")
        .arg(&binary)
        .arg("--sbom")
        .arg(&sbom)
        .args(["--platform", PLATFORM, "--commit", commit])
        .arg("--out")
        .arg(&dist)
        .output()
        .expect("run xtask");
    assert!(
        out.status.success(),
        "package: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dist.join(format!("{name}-{commit}-{PLATFORM}.tar.gz"))
}

/// A gzip tar of `entries` (name, content), each a regular file as `package` writes one,
/// to the new file `path`.
fn tarball_of(path: &Path, entries: &[(&str, &[u8])]) {
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    for (name, data) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_path(name).expect("entry name");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append(&header, *data).expect("append");
    }
    let bytes = tar.into_inner().expect("tar").finish().expect("gzip");
    std::fs::create_dir_all(path.parent().expect("a directory")).expect("dist");
    std::fs::write(path, bytes).expect("tarball");
}

/// What `build-commit check-asset <tarball> <commit>` leaves: exit code, stdout, stderr.
fn check_asset(root: &Path, tarball: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(root)
        .args(["build-commit", "check-asset"])
        .arg(tarball)
        .arg(commit)
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// As [`check_asset`], with `--plain`: only the `--version` line is checked, for a binary that
/// has no `--version --json` (kbf-updater).
fn check_asset_plain(root: &Path, tarball: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(root)
        .args(["build-commit", "check-asset", "--plain"])
        .arg(tarball)
        .arg(commit)
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// The names in the workspace's `target/` after a run: the check's scratch directory
/// must not stay there (the job uploads and caches what it finds).
fn left_in_target(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join("target"))
        .map(|dir| {
            dir.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn check_asset_passes_the_tarball_package_wrote_for_the_commit() {
    // Control: catches a check-asset that refuses the correct tarball (so every real
    // run fails), reads the wrong name out of the file name, or does not run the binary
    // it extracted; and one that leaves the extracted binary in the workspace.
    let root = scratch("ok");
    let tarball = packaged(&root, "kbf", &good_kbf(COMMIT), COMMIT);
    assert_eq!(
        check_asset(&root, &tarball, COMMIT),
        (
            Some(0),
            format!(
                "kbf-{COMMIT}-{PLATFORM}.tar.gz: ok: kbf 0.1.0+c0ffee012345; --version --json: commit c0ffee012345, profile release\n"
            ),
            String::new()
        )
    );
    assert_eq!(left_in_target(&root), Vec::<String>::new());
}

#[test]
fn check_asset_runs_the_binary_it_took_out_of_the_tarball_inside_the_workspace() {
    // Catches: the check run on some other file than the tarball's (the binary the job
    // built, a copy next to the tarball), or on one extracted outside the job's
    // workspace (the system's temporary directory, which a runner may mount noexec):
    // both `--version` forms run `<root>/target/xtask-check-asset-<pid>/kbf`.
    let root = scratch("where");
    let tarball = packaged(&root, "kbf", &good_kbf(COMMIT), COMMIT);
    // The same bytes beside the tarball and where the job built them, to be told apart.
    std::fs::copy(root.join("bin/kbf"), root.join("dist/kbf")).expect("copy");
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(&*root)
        .args(["build-commit", "check-asset"])
        .arg(&tarball)
        .arg(COMMIT)
        .env("XTASK_FAKE_ANNOUNCE", "1")
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    let prefix = format!(
        "xtask-fake: kbf runs as {}/target/xtask-check-asset-",
        root.display()
    );
    let ran: Vec<&str> = err.lines().collect();
    assert_eq!(ran.len(), 2, "plain and json forms: {err}");
    for line in ran {
        assert!(
            line.starts_with(&prefix) && line.ends_with("/kbf"),
            "{line}"
        );
    }
}

#[test]
fn check_asset_refuses_a_binary_that_names_another_commit_or_a_debug_build() {
    // Catches: the shipped tarball never re-opened (a binary stamped with another commit,
    // or a debug build, was packaged and checks made before packaging passed), the
    // plain `--version` line compared but not `--version --json`, and the extracted
    // binary left behind when the check fails.
    let other = &OTHER[..12];
    let ours = &COMMIT[..12];
    for (name, stamp, json, want) in [
        (
            "stamp",
            other,
            identity(ours, "release"),
            format!("xtask: --version is \"kbf 0.1.0+{other}\"; expected it to end +{ours}\n"),
        ),
        (
            "json-commit",
            ours,
            identity(other, "release"),
            format!("xtask: --version --json: \"commit\" is \"{other}\"; expected \"{ours}\"\n"),
        ),
        (
            "json-profile",
            ours,
            identity(ours, "debug"),
            "xtask: --version --json: \"profile\" is \"debug\"; expected \"release\"\n".to_owned(),
        ),
        (
            "json-text",
            ours,
            "kbf 0.1.0\n".to_owned(),
            "xtask: --version --json: not a JSON object: kbf 0.1.0\n".to_owned(),
        ),
    ] {
        let root = scratch(&format!("refused-{name}"));
        let tarball = packaged(&root, "kbf", &kbf_bytes(stamp, &json), COMMIT);
        assert_eq!(
            check_asset(&root, &tarball, COMMIT),
            (Some(1), String::new(), want),
            "{name}"
        );
        assert_eq!(left_in_target(&root), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn check_asset_refuses_a_tarball_of_more_than_the_one_binary() {
    // Catches: a tarball with the right binary and something else beside it passing (the
    // one-entry invariant of docs/artifacts.md: a host unpacks exactly one file), which
    // only a read of the whole archive, not a search for the entry, finds.
    let root = scratch("two-entries");
    let tarball = root
        .join("dist")
        .join(format!("kbf-{COMMIT}-{PLATFORM}.tar.gz"));
    tarball_of(
        &tarball,
        &[("kbf", &good_kbf(COMMIT)), ("README", b"not a binary")],
    );
    let (code, out, err) = check_asset(&root, &tarball, COMMIT);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    let shown = tarball.display();
    assert!(err.contains(&format!("{shown}: 2 entries, not 1")), "{err}");
    assert!(
        err.contains(&format!("{shown}: README: the name is not \"kbf\"")),
        "{err}"
    );
    assert_eq!(left_in_target(&root), Vec::<String>::new());
}

#[test]
fn check_asset_refuses_a_tarball_without_an_entry_of_the_binary_name() {
    // Catches: the binary found by anything but its exact entry name: a tarball named
    // for kbf whose one entry is another binary (kbf-server), or kbf under a directory,
    // checked as if it were kbf, though a host would not find kbf in it.
    for (name, entry) in [
        ("other-binary", "kbf-server"),
        ("in-a-directory", "bin/kbf"),
    ] {
        let root = scratch(&format!("entry-{name}"));
        let tarball = root
            .join("dist")
            .join(format!("kbf-{COMMIT}-{PLATFORM}.tar.gz"));
        tarball_of(&tarball, &[(entry, &good_kbf(COMMIT))]);
        assert_eq!(
            check_asset(&root, &tarball, COMMIT),
            (
                Some(1),
                String::new(),
                format!("xtask: {} holds no kbf\n", tarball.display())
            ),
            "{name}"
        );
        assert_eq!(left_in_target(&root), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn check_asset_refuses_a_tarball_named_for_another_commit() {
    // Catches: a tarball packaged for one commit checked as another's: its binary says
    // COMMIT, its name says OTHER, and the asset a host fetches is found by that name.
    let root = scratch("named-for-other");
    let tarball = packaged(&root, "kbf", &good_kbf(COMMIT), OTHER);
    assert_eq!(
        check_asset(&root, &tarball, COMMIT),
        (
            Some(1),
            String::new(),
            format!(
                "xtask: \"kbf-{OTHER}-{PLATFORM}.tar.gz\" is not <name>-{COMMIT}-<platform>.tar.gz\n"
            )
        )
    );
    assert_eq!(left_in_target(&root), Vec::<String>::new());
}

#[test]
fn check_asset_refuses_what_is_not_a_commit() {
    // Catches: an empty or short `$GITHUB_SHA` expansion passing (a 0-length prefix
    // matches every stamp), as `check` refuses it.
    let root = scratch("not-a-commit");
    let tarball = packaged(&root, "kbf", &good_kbf(COMMIT), COMMIT);
    for commit in ["", "c0ffee", "C0FFEE012345"] {
        assert_eq!(
            check_asset(&root, &tarball, commit),
            (
                Some(1),
                String::new(),
                format!(
                    "xtask: not a commit: {commit:?} (expected 12 to 40 lowercase hex digits)\n"
                )
            ),
            "{commit:?}"
        );
    }
}

#[test]
fn check_asset_reports_a_tarball_it_cannot_read() {
    // Catches: a tarball that is not there reported as a passed check, or as a binary
    // that names no commit.
    let root = scratch("missing");
    let tarball = root
        .join("dist")
        .join(format!("kbf-{COMMIT}-{PLATFORM}.tar.gz"));
    let (code, out, err) = check_asset(&root, &tarball, COMMIT);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with(&format!("xtask: read {}: ", tarball.display())),
        "{err}"
    );
    // Not a gzip stream at all.
    std::fs::create_dir_all(root.join("dist")).expect("dist");
    std::fs::write(&tarball, b"not a tarball").expect("write");
    let (code, out, err) = check_asset(&root, &tarball, COMMIT);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("holds no kbf"), "{err}");
    assert_eq!(left_in_target(&root), Vec::<String>::new());
}

#[test]
fn check_asset_with_a_missing_argument_exits_2() {
    // Catches: a call without the commit run as a check with an empty value.
    for args in [
        &["build-commit", "check-asset"][..],
        &["build-commit", "check-asset", "kbf.tar.gz"][..],
    ] {
        let out = Command::new(XTASK).args(args).output().expect("run");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Usage:"),
            "{args:?}"
        );
    }
}

#[test]
fn check_asset_plain_checks_the_version_line_and_leaves_the_json_alone() {
    // Catches: a plain check that still asks for `--version --json` (so kbf-updater's tarball
    // can never pass), and one that skips the `--version` line too (a binary stamped with
    // another commit would pass).
    let ours = &COMMIT[..12];
    let root = scratch("plain-ok");
    let tarball = packaged(&root, "kbf", &kbf_bytes(ours, "not json\n"), COMMIT);
    assert_eq!(
        check_asset_plain(&root, &tarball, COMMIT),
        (
            Some(0),
            format!("kbf-{COMMIT}-{PLATFORM}.tar.gz: ok: kbf 0.1.0+{ours}\n"),
            String::new()
        )
    );
    assert_eq!(left_in_target(&root), Vec::<String>::new());
    let other = &OTHER[..12];
    let root = scratch("plain-refused");
    let tarball = packaged(&root, "kbf", &kbf_bytes(other, "not json\n"), COMMIT);
    assert_eq!(
        check_asset_plain(&root, &tarball, COMMIT),
        (
            Some(1),
            String::new(),
            format!("xtask: --version is \"kbf 0.1.0+{other}\"; expected it to end +{ours}\n")
        )
    );
}
