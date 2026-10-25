//! `cargo xtask pkg-config`, `pkg-build`, `pkg-install-test` and `pkg-upgrade-test` run as CI
//! runs them, over copies of the repository's packaging files and the test-only
//! `kbf-installer-stub`, which stands for the `kbf` binary. The programs nfpm and docker are
//! `false`: what is tested here is the command line and how a failing program is reported;
//! the packages themselves are built, installed and upgraded by CI (the `packages` job and the
//! Linux jobs of `artifacts.yml`), and the logic is tested in `src/pkg_linux/tests.rs`,
//! `src/pkg_install/tests.rs` and `src/pkg_upgrade/tests.rs`.

use kbf_testkit::dir::TestDir;
use std::path::Path;
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const STUB: &str = env!("CARGO_BIN_EXE_kbf-installer-stub");
const FALSE: &str = "/usr/bin/false";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pkg"),
        name,
    )
}

/// Copies the repository's packaging inputs and the stub into `dir`.
fn copy_inputs(dir: &Path) {
    let packaging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    for sub in ["testdata/units", "testdata/trust"] {
        let to = dir.join(sub);
        std::fs::create_dir_all(&to).expect("dir");
        for entry in std::fs::read_dir(packaging.join(sub)).expect("read") {
            let entry = entry.expect("entry");
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
        }
    }
    for file in [
        "sysusers.d/kbf-installer.conf",
        "tmpfiles.d/kbf-installer.conf",
        "needrestart/kbf.conf",
    ] {
        std::fs::copy(packaging.join(file), dir.join(file.replace('/', "-"))).expect("copy");
    }
    std::fs::copy(STUB, dir.join("kbf")).expect("stub");
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn xtask(args: &[&str]) -> Output {
    Command::new(XTASK).args(args).output().expect("run xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The arguments of `pkg-config` over the copies in `dir` (see [`copy_inputs`]), into `out`,
/// without a version.
fn config_args(dir: &Path, out: &Path) -> Vec<String> {
    let at = |rel: &str| s(&dir.join(rel)).to_owned();
    [
        "pkg-config",
        "--kbf",
        &at("kbf"),
        "--units",
        &at("testdata/units"),
        "--trust",
        &at("testdata/trust"),
        "--sysusers",
        &at("sysusers.d-kbf-installer.conf"),
        "--tmpfiles",
        &at("tmpfiles.d-kbf-installer.conf"),
        "--needrestart",
        &at("needrestart-kbf.conf"),
        "--platform",
        "linux-x86_64",
        "--out",
        s(out),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Runs xtask with `args`.
fn xtask_with(args: &[String]) -> Output {
    Command::new(XTASK).args(args).output().expect("run xtask")
}

/// `pkg-config` over the copies in `dir`, into `dir/out`.
fn config(dir: &Path, version: &str) -> Output {
    copy_inputs(dir);
    let mut args = config_args(dir, &dir.join("out"));
    args.extend(["--version".to_owned(), version.to_owned()]);
    xtask_with(&args)
}

#[test]
fn the_stub_prints_its_arguments_and_exits_zero() {
    let out = Command::new(STUB)
        .args(["boot-check", "--x"])
        .output()
        .expect("stub");
    assert!(out.status.success());
    assert_eq!(
        text(&out.stdout),
        "kbf-installer-stub (test only) boot-check --x\n"
    );
}

/// Asserts that `pkg-config` writes the configuration and the listing of the `kbf` package in
/// the output directory: the binary, the five mode units, the slice, the sysusers and tmpfiles
/// files, the needrestart drop-in and the three trust files, the recommendations of the deb
/// and the rpm (no dependency), the replacement of `kbf-installer` in the deb only (no break), and none of the
/// retired installer's files.
#[test]
fn pkg_config_writes_the_kbf_package() {
    let dir = scratch("config");
    let out = config(&dir, "0.0.0+ci");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml = std::fs::read_to_string(dir.join("out/nfpm.yaml")).expect("yaml");
    assert!(yaml.contains("name: \"kbf\"\n"), "{yaml}");
    assert!(yaml.contains("dst: \"/usr/bin/kbf\""), "{yaml}");
    assert!(
        yaml.contains(
            "recommends:\n  - \"crun\"\n  - \"uidmap\"\n  - \"systemd\"\n  - \"ca-certificates\"\n"
        ),
        "{yaml}"
    );
    assert!(!yaml.contains("depends"), "{yaml}");
    let replaces = "  deb:\n    replaces:\n      - \"kbf-installer (<< 0.1.1)\"\n";
    assert!(yaml.contains(replaces), "{yaml}");
    assert!(!yaml.contains("breaks"), "{yaml}");
    let listing = std::fs::read_to_string(dir.join("out/listing.txt")).expect("listing");
    assert!(listing.contains(" 0755 /usr/bin/kbf "), "{listing}");
    for unit in [
        "all-in-one",
        "daemon",
        "frontend",
        "storage",
        "storage-and-daemon",
    ] {
        let path = format!(" /usr/lib/systemd/system/kbf-{unit}.service ");
        assert!(listing.contains(&path), "{path} in {listing}");
    }
    for path in [
        " /usr/lib/systemd/system/kbf.slice ",
        " /usr/lib/sysusers.d/kbf.conf ",
        " /usr/lib/tmpfiles.d/kbf.conf ",
        " /etc/needrestart/conf.d/kbf.conf ",
        " 0755 /usr/share/kbf/trust/gh ",
        " 0644 /usr/share/kbf/trust/installer.conf ",
        " 0644 /usr/share/kbf/trust/trusted_root.jsonl ",
    ] {
        assert!(listing.contains(path), "{path} in {listing}");
    }
    assert!(!listing.contains("kbf-installer"), "{listing}");
    assert!(!listing.contains("kbf-node.service"), "{listing}");
    assert_eq!(listing.lines().count(), 15, "{listing}");
}

/// Asserts that `pkg-config` without `--kbf` is a usage error and writes nothing, and that
/// the `--binary` of the installer package it once took is not a flag any more.
#[test]
fn pkg_config_requires_the_kbf_binary_and_takes_no_installer() {
    let dir = scratch("config-no-kbf");
    copy_inputs(&dir);
    let mut args = config_args(&dir, &dir.join("out"));
    args.extend(["--version".to_owned(), "0.0.0+ci".to_owned()]);
    let at = args.iter().position(|a| a == "--kbf").expect("--kbf");
    let without: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != at && *i != at + 1)
        .map(|(_, a)| a.clone())
        .collect();
    let out = xtask_with(&without);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--kbf"), "{}", text(&out.stderr));
    assert!(!dir.join("out").exists());
    let mut with_binary = args;
    with_binary.extend(["--binary".to_owned(), s(&dir.join("kbf")).to_owned()]);
    let out = xtask_with(&with_binary);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("--binary"),
        "{}",
        text(&out.stderr)
    );
    assert!(!dir.join("out").exists());
}

#[test]
fn pkg_config_refuses_a_bad_version_with_every_problem() {
    let dir = scratch("config-refused");
    let out = config(&dir, "v 1");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("version \"v 1\""),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn pkg_build_and_pkg_install_test_report_a_program_that_fails() {
    let dir = scratch("programs");
    assert_eq!(config(&dir, "0.0.0+ci").status.code(), Some(0));
    let config = dir.join("out/nfpm.yaml");
    let build = xtask(&[
        "pkg-build",
        "--nfpm",
        FALSE,
        "--config",
        s(&config),
        "--out",
        s(&dir.join("pkgs")),
        "--source-date-epoch",
        "5",
    ]);
    assert_eq!(build.status.code(), Some(1));
    assert!(
        text(&build.stderr).contains("nfpm package -p deb"),
        "{}",
        text(&build.stderr)
    );
    let image = dir.join("image");
    std::fs::create_dir_all(&image).expect("image dir");
    let install = xtask(&[
        "pkg-install-test",
        "--name",
        "kbf",
        "--package",
        s(&dir.join("kbf.deb")),
        "--format",
        "deb",
        "--listing",
        s(&dir.join("out/listing.txt")),
        "--image-dir",
        s(&image),
        "--gate",
        "kbf-node.service=/etc/kbf/kbf.toml",
        "--docker",
        FALSE,
    ]);
    assert_eq!(install.status.code(), Some(1));
    assert!(
        text(&install.stderr).contains("docker build"),
        "{}",
        text(&install.stderr)
    );
}

/// Asserts that `pkg-upgrade-test` runs docker as the install test does and reports it when
/// it fails, naming the step, and that it needs the debs of the release before.
#[test]
fn pkg_upgrade_test_reports_a_docker_that_fails_and_needs_the_old_debs() {
    let dir = scratch("upgrade");
    assert_eq!(config(&dir, "0.0.0+ci").status.code(), Some(0));
    let image = dir.join("image");
    std::fs::create_dir_all(&image).expect("image dir");
    let deb = |name: &str| {
        let path = dir.join(name);
        std::fs::write(&path, b"deb").expect("deb");
        path
    };
    let (old_installer, old_kbf, new_kbf) = (
        deb("kbf-installer-old.deb"),
        deb("kbf-old.deb"),
        deb("kbf-new.deb"),
    );
    let args = |docker: &'static str| {
        xtask(&[
            "pkg-upgrade-test",
            "--old-installer",
            s(&old_installer),
            "--old-kbf",
            s(&old_kbf),
            "--package",
            s(&new_kbf),
            "--listing",
            s(&dir.join("out/listing.txt")),
            "--image-dir",
            s(&image),
            "--docker",
            docker,
        ])
    };
    let run = args(FALSE);
    assert_eq!(run.status.code(), Some(1), "{}", text(&run.stderr));
    assert!(
        text(&run.stderr).contains("docker build"),
        "{}",
        text(&run.stderr)
    );
    let missing = xtask(&["pkg-upgrade-test", "--package", s(&new_kbf)]);
    assert_eq!(missing.status.code(), Some(2), "{}", text(&missing.stderr));
    assert!(
        text(&missing.stderr).contains("--old-installer"),
        "{}",
        text(&missing.stderr)
    );
}

#[test]
fn pkg_config_takes_the_version_from_the_binary_or_from_the_flag_and_not_both() {
    // Catches: `--version-of` ignored (the package version then has no commit in it), a
    // binary that is not named for what it prints (the stub prints another line) accepted as
    // the source of the version, and both flags, or neither, accepted.
    let dir = scratch("config-version-of");
    copy_inputs(&dir);
    // The fake, run as `kbf` with a stamp, names itself as the real one does.
    let named = dir.join("named");
    std::fs::create_dir(&named).expect("dir");
    let kbf = named.join("kbf");
    std::fs::hard_link(env!("CARGO_BIN_EXE_xtask-fake"), &kbf).expect("link the fake");
    std::fs::write(named.join("kbf.stamp"), "0123456789ab").expect("stamp");
    std::fs::write(named.join("kbf.json"), "{}\n").expect("json");
    let run = |extra: &[&str], out: &str| {
        let mut args = config_args(&dir, &dir.join(out));
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        xtask_with(&args)
    };
    let ok = run(&["--version-of", s(&kbf)], "of");
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok.stderr));
    let yaml = std::fs::read_to_string(dir.join("of/nfpm.yaml")).expect("yaml");
    assert!(yaml.contains("version: \"0.1.0+0123456789ab\""), "{yaml}");
    // The stub, copied as `kbf`, prints another line: it is no source of a version.
    let stub = run(&["--version-of", s(&dir.join("kbf"))], "stub");
    assert_eq!(stub.status.code(), Some(1));
    assert!(
        text(&stub.stderr).contains("not `kbf <version>+<commit>`"),
        "{}",
        text(&stub.stderr)
    );
    assert!(!dir.join("stub").exists());
    let both = run(&["--version", "1.0.0", "--version-of", s(&kbf)], "both");
    assert_eq!(both.status.code(), Some(2), "{}", text(&both.stderr));
    let neither = run(&[], "neither");
    assert_eq!(neither.status.code(), Some(2), "{}", text(&neither.stderr));
    assert!(
        text(&neither.stderr).contains("--version or --version-of"),
        "{}",
        text(&neither.stderr)
    );
    assert!(!dir.join("neither").exists());
}

#[test]
fn the_fake_installer_answers_only_a_lone_version_flag_with_the_stamp() {
    // Catches: the stamped `--version` answer given to any single argument (a stub that
    // prints a version for `boot-check` hides the real installer's behaviour from the
    // tests that run it).
    let dir = scratch("fake-installer");
    let installer = dir.join("kbf-installer");
    std::fs::hard_link(env!("CARGO_BIN_EXE_xtask-fake"), &installer).expect("link the fake");
    let run = |arg: &str| {
        Command::new(&installer)
            .arg(arg)
            .env("XTASK_FAKE_INSTALLER_VERSION", "0123456789ab")
            .output()
            .expect("run the fake")
    };
    assert_eq!(
        text(&run("--version").stdout),
        "kbf-installer 0.1.0+0123456789ab\n"
    );
    let other = run("boot-check");
    assert!(other.status.success());
    assert_eq!(
        text(&other.stdout),
        "kbf-installer-stub boot-check\n",
        "{}",
        text(&other.stderr)
    );
}

#[test]
fn pkg_install_test_names_a_directory_without_the_package_of_its_format() {
    // Catches: `--format rpm` looking for a `.deb` (or the reverse), and a directory with
    // no package of the format going on to docker.
    let dir = scratch("install-no-package");
    let pkgs = dir.join("pkgs");
    std::fs::create_dir_all(&pkgs).expect("pkgs");
    std::fs::write(pkgs.join("kbf_1_amd64.deb"), b"deb").expect("deb");
    let args = |format: &'static str| {
        let (pkgs, dir) = (s(&pkgs).to_owned(), s(&dir).to_owned());
        move || {
            xtask(&[
                "pkg-install-test",
                "--name",
                "kbf",
                "--package",
                &pkgs,
                "--format",
                format,
                "--listing",
                &format!("{dir}/listing.txt"),
                "--image-dir",
                &dir,
                "--docker",
                FALSE,
            ])
        }
    };
    let rpm = args("rpm")();
    assert_eq!(rpm.status.code(), Some(1), "{}", text(&rpm.stderr));
    let said = text(&rpm.stderr);
    assert!(said.contains("expected one .rpm file in"), "{said}");
    // The deb is found, so the run goes on to docker, which is `false`.
    let deb = args("deb")();
    assert!(
        text(&deb.stderr).contains("docker") || text(&deb.stderr).contains("listing"),
        "{}",
        text(&deb.stderr)
    );
    assert!(!text(&deb.stderr).contains("expected one"));
}

fn tgz(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let gzip = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    for (path, mode, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(format!("./{path}")).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(*mode);
        h.set_cksum();
        tar.append(&h, *bytes).expect("append");
    }
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// The control file of a good `kbf` deb: the recommendations, the suggestion, and the replacement
/// of the old installer (no dependency, no break).
const KBF_CONTROL: &[u8] = b"Package: kbf\nVersion: 0.1.0\nArchitecture: amd64\n\
                             Recommends: crun, uidmap, systemd, ca-certificates\n\
                             Suggests: postgresql\n\
                             Replaces: kbf-installer (<< 0.1.1)\n";

/// A deb as nfpm writes the `kbf` package, holding `kbf`, the unit of every mode, the slice, the
/// sysusers, tmpfiles and needrestart files, and `gh` and the trust files for `source_ref`.
fn kbf_deb_of(kbf: &[u8], source_ref: &str) -> Vec<u8> {
    let control = tgz(&[("control", 0o644, KBF_CONTROL)]);
    let conf = format!("SOURCE_REFS={source_ref}\n");
    let mut files: Vec<(&str, u32, &[u8])> = vec![("usr/bin/kbf", 0o755, kbf)];
    for unit in [
        "usr/lib/systemd/system/kbf-all-in-one.service",
        "usr/lib/systemd/system/kbf-daemon.service",
        "usr/lib/systemd/system/kbf-frontend.service",
        "usr/lib/systemd/system/kbf-storage-and-daemon.service",
        "usr/lib/systemd/system/kbf-storage.service",
        "usr/lib/systemd/system/kbf.slice",
        "usr/lib/sysusers.d/kbf.conf",
        "usr/lib/tmpfiles.d/kbf.conf",
        "etc/needrestart/conf.d/kbf.conf",
    ] {
        files.push((unit, 0o644, b"[Unit]\n"));
    }
    files.push(("usr/share/kbf/trust/gh", 0o755, b"the pinned gh"));
    files.push(("usr/share/kbf/trust/installer.conf", 0o644, conf.as_bytes()));
    files.push((
        "usr/share/kbf/trust/trusted_root.jsonl",
        0o644,
        b"{\"mediaType\":\"trustedroot\"}\n",
    ));
    deb_from(&control, &tgz(&files))
}

/// The `ar` archive of `control` and `data`, the tarballs of a deb.
fn deb_from(control: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, body) in [
        ("debian-binary", &b"2.0\n"[..]),
        ("control.tar.gz", control),
        ("data.tar.gz", data),
    ] {
        out.extend(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                "100644",
                body.len()
            )
            .as_bytes(),
        );
        out.extend(body);
        if body.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const MAIN: &str = "refs/heads/main";

#[test]
fn pkg_trust_writes_the_trust_directory_and_refuses_a_ref_no_host_would_load() {
    // Catches: the verb not wired to its writer (no file, or `installer.conf` of another
    // ref), a mode other than 0755 for `gh`, and a branch outside `refs/heads/` accepted.
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("trust");
    let (gh, root) = (dir.join("gh"), dir.join("root.jsonl"));
    std::fs::write(&gh, b"the pinned gh").expect("gh");
    std::fs::write(&root, b"{\"mediaType\":\"trustedroot\"}\n").expect("root");
    let out = dir.join("trust");
    let run = |source_ref: &str| {
        xtask(&[
            "pkg-trust",
            "--gh",
            s(&gh),
            "--trusted-root",
            s(&root),
            "--source-ref",
            source_ref,
            "--out",
            s(&out),
        ])
    };
    let bad = run("main");
    assert_eq!(bad.status.code(), Some(1), "{}", text(&bad.stderr));
    assert!(!out.exists());
    let good = run("refs/heads/team/topic-2");
    assert_eq!(good.status.code(), Some(0), "{}", text(&good.stderr));
    assert_eq!(
        std::fs::read_to_string(out.join("installer.conf")).expect("conf"),
        "SOURCE_REFS=refs/heads/team/topic-2\n"
    );
    assert_eq!(std::fs::read(out.join("gh")).expect("gh"), b"the pinned gh");
    let mode = std::fs::metadata(out.join("gh"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

/// Asserts that the installer's deb is no longer an asset: `pkg-asset` is not a verb.
#[test]
fn the_installer_deb_has_no_asset_verb() {
    let out = xtask(&["pkg-asset", "--verify-only"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("pkg-asset"),
        "{}",
        text(&out.stderr)
    );
}

/// Asserts that `pkg-kbf-asset` adds the `kbf` deb as `kbf-<commit>-<platform>.deb` with its
/// line in the sums, only verifies it with `--verify-only` and writes nothing then, and
/// refuses, in either mode and writing nothing, a deb whose `kbf` is not the attested one
/// and an `--out` given with `--verify-only`.
#[test]
fn pkg_kbf_asset_adds_the_package_as_the_asset_or_only_verifies_it() {
    let dir = scratch("kbf-asset");
    let kbf: &[u8] = b"\x7fELF the kbf";
    let tarball = dir.join(format!("kbf-{COMMIT}-linux-x86_64.tar.gz"));
    std::fs::write(&tarball, tgz(&[("kbf", 0o755, kbf)])).expect("tarball");
    let pkgs = dir.join("pkgs");
    std::fs::create_dir_all(&pkgs).expect("pkgs");
    std::fs::write(pkgs.join("kbf_0.1.0_amd64.deb"), kbf_deb_of(kbf, MAIN)).expect("deb");
    let out = dir.join("dist");
    let run = |package: &Path, mode: &[&str]| {
        let mut args = vec![
            "pkg-kbf-asset",
            "--package",
            s(package),
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
            "--kbf-tarball",
            s(&tarball),
            "--expect-ref",
            MAIN,
        ];
        args.extend(mode);
        xtask(&args)
    };
    let name = format!("kbf-{COMMIT}-linux-x86_64.deb");
    let added = run(&pkgs, &["--out", s(&out)]);
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
    assert!(out.join(&name).is_file());
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    assert!(sums.trim_end().ends_with(&format!("  {name}")), "{sums}");
    let before = std::fs::read_dir(&out).expect("dist").count();
    let verified = run(&out.join(&name), &["--verify-only"]);
    assert_eq!(
        verified.status.code(),
        Some(0),
        "{}",
        text(&verified.stderr)
    );
    assert!(
        text(&verified.stdout).contains(&name),
        "{}",
        text(&verified.stdout)
    );
    assert_eq!(std::fs::read_dir(&out).expect("dist").count(), before);
    let other = dir.join("other");
    std::fs::create_dir_all(&other).expect("other");
    std::fs::write(other.join("p.deb"), kbf_deb_of(b"another kbf", MAIN)).expect("deb");
    let fresh = dir.join("fresh");
    let refused = run(&other, &["--out", s(&fresh)]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    assert!(text(&refused.stderr).contains("is not the binary of"));
    assert!(!fresh.exists());
    let refused = run(&other.join("p.deb"), &["--verify-only"]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    let both = run(&pkgs, &["--out", s(&fresh), "--verify-only"]);
    assert_eq!(both.status.code(), Some(2), "{}", text(&both.stderr));
    // A deb whose trust directory is for another branch than the one it is attested on.
    let wrong = dir.join("wrong");
    std::fs::create_dir_all(&wrong).expect("wrong");
    let feature = "refs/heads/feature/arch-v2";
    std::fs::write(wrong.join("p.deb"), kbf_deb_of(kbf, feature)).expect("deb");
    let refused = run(&wrong, &["--out", s(&fresh)]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    assert!(
        text(&refused.stderr).contains("but this package is attested for refs/heads/main"),
        "{}",
        text(&refused.stderr)
    );
    assert!(!fresh.exists());
}

/// Asserts that `pkg-kbf-asset` names a directory without a deb, and refuses a deb that is
/// not the `kbf` package (another name, no `kbf`, none of its files), saying so.
#[test]
fn pkg_kbf_asset_names_a_directory_without_a_deb_and_refuses_another_package() {
    let dir = scratch("kbf-asset-other");
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("dir");
    let tarball = dir.join(format!("kbf-{COMMIT}-linux-x86_64.tar.gz"));
    std::fs::write(&tarball, tgz(&[("kbf", 0o755, b"k")])).expect("tarball");
    let run = |package: &Path| {
        xtask(&[
            "pkg-kbf-asset",
            "--package",
            s(package),
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
            "--kbf-tarball",
            s(&tarball),
            "--expect-ref",
            MAIN,
            "--verify-only",
        ])
    };
    let none = run(&empty);
    assert_eq!(none.status.code(), Some(1), "{}", text(&none.stderr));
    assert!(text(&none.stderr).contains("expected one .deb file in"));
    let other = dir.join(format!("kbf-{COMMIT}-linux-x86_64.deb"));
    let control = tgz(&[(
        "control",
        0o644,
        b"Package: kbf-installer\nArchitecture: amd64\n",
    )]);
    let data = tgz(&[("usr/bin/kbf-installer", 0o755, b"k")]);
    std::fs::write(&other, deb_from(&control, &data)).expect("deb");
    let refused = run(&other);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    let said = text(&refused.stderr);
    let package = "control says Package \"kbf-installer\", not \"kbf\"";
    assert!(said.contains(package), "{said}");
    assert!(said.contains("holds no usr/bin/kbf"), "{said}");
    assert!(
        said.contains("holds usr/bin/kbf-installer, which the kbf package does not ship"),
        "{said}"
    );
}
