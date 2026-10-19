//! `cargo xtask pkg-config`, `pkg-build` and `pkg-install-test` run as CI runs them, over
//! copies of the repository's packaging files and the test-only `kbf-installer-stub`.
//! The programs nfpm and docker are `false`: what is tested here is the command line and
//! how a failing program is reported; the packages themselves are built and installed
//! by CI (the `packages` job), and the logic is tested in `src/pkg_linux/tests.rs` and
//! `src/pkg_install/tests.rs`.

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const STUB: &str = env!("CARGO_BIN_EXE_kbf-installer-stub");
const FALSE: &str = "/usr/bin/false";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pkg"),
        name,
    );
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
