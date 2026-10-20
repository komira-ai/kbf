//! `cargo xtask pkg-config`, `pkg-build` and `pkg-install-test` run as CI runs them, over
//! copies of the repository's packaging files and the test-only `kbf-installer-stub`.
//! The programs nfpm and docker are `false`: what is tested here is the command line and
//! how a failing program is reported; the packages themselves are built and installed
//! by CI (the `packages` job), and the logic is tested in `src/pkg_linux/tests.rs` and
//! `src/pkg_install/tests.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const STUB: &str = env!("CARGO_BIN_EXE_kbf-installer-stub");
const FALSE: &str = "/usr/bin/false";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-pkg")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
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
    ] {
        std::fs::copy(packaging.join(file), dir.join(file.replace('/', "-"))).expect("copy");
    }
    std::fs::copy(STUB, dir.join("kbf-installer")).expect("stub");
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

/// `pkg-config` over the copies in `dir`, into `dir/out`.
fn config(dir: &Path, version: &str) -> Output {
    copy_inputs(dir);
    let out = dir.join("out");
    xtask(&[
        "pkg-config",
        "--binary",
        s(&dir.join("kbf-installer")),
        "--units",
        s(&dir.join("testdata/units")),
        "--trust",
        s(&dir.join("testdata/trust")),
        "--sysusers",
        s(&dir.join("sysusers.d-kbf-installer.conf")),
        "--tmpfiles",
        s(&dir.join("tmpfiles.d-kbf-installer.conf")),
        "--platform",
        "linux-x86_64",
        "--version",
        version,
        "--out",
        s(&out),
    ])
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

#[test]
fn pkg_config_writes_the_configuration_and_the_listing() {
    let dir = scratch("config");
    let out = config(&dir, "0.0.0+ci");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml = std::fs::read_to_string(dir.join("out/nfpm.yaml")).expect("yaml");
    assert!(yaml.contains("dst: \"/usr/bin/kbf-installer\""), "{yaml}");
    let listing = std::fs::read_to_string(dir.join("out/listing.txt")).expect("listing");
    assert!(
        listing.contains(" /usr/lib/systemd/system/kbf-node.service "),
        "{listing}"
    );
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
        "--package",
        s(&dir.join("kbf-installer.deb")),
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

#[test]
fn pkg_config_takes_the_version_from_the_binary_or_from_the_flag_and_not_both() {
    // Catches: `--version-of` ignored (the package version then has no commit in it), a
    // binary that is not the installer (the stub prints another line) accepted as the
    // source of the version, and both flags, or neither, accepted.
    let dir = scratch("config-version-of");
    copy_inputs(&dir);
    // The fake, run as `kbf-installer` with a stamp, names itself as the real one does.
    let installer = dir.join("named").join("kbf-installer");
    std::fs::create_dir(dir.join("named")).expect("dir");
    std::fs::hard_link(env!("CARGO_BIN_EXE_xtask-fake"), &installer).expect("link the fake");
    let base = |dir: &Path, out: &str| {
        vec![
            "pkg-config".to_owned(),
            "--binary".to_owned(),
            s(&dir.join("kbf-installer")).to_owned(),
            "--units".to_owned(),
            s(&dir.join("testdata/units")).to_owned(),
            "--trust".to_owned(),
            s(&dir.join("testdata/trust")).to_owned(),
            "--sysusers".to_owned(),
            s(&dir.join("sysusers.d-kbf-installer.conf")).to_owned(),
            "--tmpfiles".to_owned(),
            s(&dir.join("tmpfiles.d-kbf-installer.conf")).to_owned(),
            "--platform".to_owned(),
            "linux-x86_64".to_owned(),
            "--out".to_owned(),
            s(&dir.join(out)).to_owned(),
        ]
    };
    let run = |extra: &[&str], out: &str| {
        let mut args = base(&dir, out);
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        Command::new(XTASK)
            .args(&args)
            .env("XTASK_FAKE_INSTALLER_VERSION", "0123456789ab")
            .output()
            .expect("run xtask")
    };
    let ok = run(&["--version-of", s(&installer)], "of");
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok.stderr));
    let yaml = std::fs::read_to_string(dir.join("of/nfpm.yaml")).expect("yaml");
    assert!(yaml.contains("version: \"0.1.0+0123456789ab\""), "{yaml}");
    let stub = run(&["--version-of", s(&dir.join("kbf-installer"))], "stub");
    assert_eq!(stub.status.code(), Some(1));
    assert!(
        text(&stub.stderr).contains("not `kbf-installer <version>+<commit>`"),
        "{}",
        text(&stub.stderr)
    );
    assert!(!dir.join("stub").exists());
    let both = run(
        &["--version", "1.0.0", "--version-of", s(&installer)],
        "both",
    );
    assert_eq!(both.status.code(), Some(2), "{}", text(&both.stderr));
    let neither = run(&[], "neither");
    assert_eq!(neither.status.code(), Some(2), "{}", text(&neither.stderr));
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
    std::fs::write(pkgs.join("kbf-installer_1_amd64.deb"), b"deb").expect("deb");
    let args = |format: &'static str| {
        let (pkgs, dir) = (s(&pkgs).to_owned(), s(&dir).to_owned());
        move || {
            xtask(&[
                "pkg-install-test",
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

/// A deb as nfpm writes one, holding `binary`, a `gh`, and the trust files for `source_ref`.
fn deb_of(binary: &[u8], source_ref: &str) -> Vec<u8> {
    let control = tgz(&[(
        "control",
        0o644,
        b"Package: kbf-installer\nArchitecture: amd64\n",
    )]);
    let conf = format!("SOURCE_REFS={source_ref}\n");
    let data = tgz(&[
        ("usr/bin/kbf-installer", 0o755, binary),
        ("usr/share/kbf-installer/trust/gh", 0o755, b"the pinned gh"),
        (
            "usr/share/kbf-installer/trust/installer.conf",
            0o644,
            conf.as_bytes(),
        ),
        (
            "usr/share/kbf-installer/trust/trusted_root.jsonl",
            0o644,
            b"{\"mediaType\":\"trustedroot\"}\n",
        ),
    ]);
    let mut out = b"!<arch>\n".to_vec();
    for (name, body) in [
        ("debian-binary", &b"2.0\n"[..]),
        ("control.tar.gz", &control),
        ("data.tar.gz", &data),
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
    let good = run("refs/heads/feature/arch-v2");
    assert_eq!(good.status.code(), Some(0), "{}", text(&good.stderr));
    assert_eq!(
        std::fs::read_to_string(out.join("installer.conf")).expect("conf"),
        "SOURCE_REFS=refs/heads/feature/arch-v2\n"
    );
    assert_eq!(std::fs::read(out.join("gh")).expect("gh"), b"the pinned gh");
    let mode = std::fs::metadata(out.join("gh"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn pkg_asset_adds_the_package_as_the_asset_or_only_verifies_it() {
    // Catches: `--out` and `--verify-only` swapped (the verify job writing into a
    // directory, the build job writing nothing), the package found by directory not
    // reaching the check, a refused package leaving an asset behind, and a deb that is
    // not the attested binary's accepted.
    let dir = scratch("asset");
    let binary: &[u8] = b"\x7fELF the kbf-installer";
    let tarball = dir.join(format!("kbf-installer-{COMMIT}-linux-x86_64.tar.gz"));
    std::fs::write(&tarball, tgz(&[("kbf-installer", 0o755, binary)])).expect("tarball");
    let pkgs = dir.join("pkgs");
    std::fs::create_dir_all(&pkgs).expect("pkgs");
    std::fs::write(
        pkgs.join("kbf-installer_0.1.0_amd64.deb"),
        deb_of(binary, MAIN),
    )
    .expect("deb");
    let out = dir.join("dist");
    let run = |package: &Path, mode: &[&str]| {
        let mut args = vec![
            "pkg-asset",
            "--package",
            s(package),
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
            "--binary-tarball",
            s(&tarball),
            "--expect-ref",
            MAIN,
        ];
        args.extend(mode);
        xtask(&args)
    };
    let name = format!("kbf-installer-{COMMIT}-linux-x86_64.deb");
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
    // A package of another binary: refused by either mode, and nothing is written.
    let other = dir.join("other");
    std::fs::create_dir_all(&other).expect("other");
    std::fs::write(other.join("p.deb"), deb_of(b"another binary", MAIN)).expect("deb");
    let fresh = dir.join("fresh");
    let refused = run(&other, &["--out", s(&fresh)]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    assert!(
        text(&refused.stderr).contains("is not the binary of"),
        "{}",
        text(&refused.stderr)
    );
    assert!(!fresh.exists());
    let refused = run(&other.join("p.deb"), &["--verify-only"]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
}
