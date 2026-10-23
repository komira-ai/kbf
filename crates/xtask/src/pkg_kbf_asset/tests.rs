use std::path::PathBuf;

use super::*;
use crate::fetch::Sha256;
use crate::package::tarball as binary_tarball;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.1.0+0123456789ab";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-kbf-asset-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The tar.gz of `files` as nfpm writes a data or control tarball: `./`-prefixed paths.
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

/// An `ar` archive of `members`, as `ar` and dpkg-deb write one.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, body) in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            "100644",
            body.len()
        );
        out.extend(header.as_bytes());
        out.extend(*body);
        if body.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The path of the unit `unit` in the data tarball.
fn in_units(unit: &str) -> String {
    format!("usr/lib/systemd/system/{unit}")
}

/// What a good `kbf` package holds, each of which a test may change.
struct Fix {
    kbf: Vec<u8>,
    kbf_mode: u32,
    unit_mode: u32,
    package: &'static str,
    version: &'static str,
    arch: &'static str,
    depends: String,
    replaces: String,
    drop: Option<String>,
    /// Files a good package does not hold, with their modes and bytes.
    extra: Vec<(String, u32, Vec<u8>)>,
}

impl Fix {
    fn new() -> Fix {
        Fix {
            kbf: b"\x7fELF the kbf built for the commit".to_vec(),
            kbf_mode: 0o755,
            unit_mode: 0o644,
            package: "kbf",
            version: VERSION,
            arch: "amd64",
            depends: format!("kbf-installer (>= {VERSION})"),
            replaces: "kbf-installer".to_owned(),
            drop: None,
            extra: Vec::new(),
        }
    }

    /// The deb: nfpm's layout, gzip members, `./` paths.
    fn deb(&self) -> Vec<u8> {
        let control = format!(
            "Package: {}\nVersion: {}\nArchitecture: {}\nDepends: {}\nReplaces: {}\n",
            self.package, self.version, self.arch, self.depends, self.replaces
        );
        let control_tgz = tgz(&[("control", 0o644, control.as_bytes())]);
        let binary = "usr/bin/kbf".to_owned();
        let mut files = vec![(binary, self.kbf_mode, self.kbf.as_slice())];
        for unit in KBF_UNITS {
            files.push((in_units(unit), self.unit_mode, unit.as_bytes()));
        }
        files.retain(|(p, _, _)| Some(p) != self.drop.as_ref());
        for (path, mode, bytes) in &self.extra {
            files.push((path.clone(), *mode, bytes.as_slice()));
        }
        let listed: Vec<(&str, u32, &[u8])> = files
            .iter()
            .map(|(p, m, b)| (p.as_str(), *m, *b))
            .collect();
        ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", &control_tgz),
            ("data.tar.gz", &tgz(&listed)),
        ])
    }
}

/// A directory with the attested tarball of `kbf` for `platform` and the deb of `fix`:
/// (dir, deb path, tarball path).
fn world(name: &str, fix: &Fix, kbf: &[u8], platform: Platform) -> (PathBuf, PathBuf, PathBuf) {
    let dir = scratch(name);
    let tarball = dir.join(format!("kbf-{COMMIT}-{}.tar.gz", platform.name()));
    std::fs::write(&tarball, binary_tarball("kbf", kbf).expect("tarball")).expect("tarball");
    let deb = dir.join(format!("kbf_{VERSION}_amd64.deb"));
    std::fs::write(&deb, fix.deb()).expect("deb");
    (dir, deb, tarball)
}

fn check(platform: Platform, tarball: &Path) -> Check<'_> {
    Check {
        platform,
        kbf_tarball: tarball,
    }
}

/// The problems of `fix` as the `kbf` package for x86_64.
fn found(name: &str, fix: &Fix) -> Vec<String> {
    let (_, deb, tarball) = world(name, fix, &Fix::new().kbf, Platform::LinuxX86_64);
    problems(
        &std::fs::read(&deb).expect("deb"),
        &check(Platform::LinuxX86_64, &tarball),
    )
    .expect("problems")
}

/// Asserts that a package of the attested `kbf`, depending on its own installer and replacing
/// it, with the unit of every mode, is accepted as the asset for either platform: copied under
/// a name of the commit and platform, with its digest in the sums file.
#[test]
fn a_package_of_the_attested_kbf_with_the_mode_units_is_accepted_as_the_asset() {
    for (platform, arch) in [
        (Platform::LinuxX86_64, "amd64"),
        (Platform::LinuxArm64, "arm64"),
    ] {
        let mut fix = Fix::new();
        fix.arch = arch;
        let (dir, deb, tarball) = world("good", &fix, &fix.kbf, platform);
        let out = dir.join("dist");
        let c = check(platform, &tarball);
        let printed = asset(&deb, COMMIT, &Mode::Add(&out), &c).expect("asset");
        let name = format!("kbf-{COMMIT}-{}.deb", platform.name());
        let bytes = std::fs::read(&deb).expect("deb");
        assert_eq!(std::fs::read(out.join(&name)).expect("asset"), bytes);
        let line = format!("{}  {name}\n", Sha256::of(&bytes));
        let sums = out.join(format!("SHA256SUMS.{}", platform.name()));
        assert_eq!(std::fs::read_to_string(sums).expect("sums"), line);
        assert_eq!(
            printed,
            vec![format!("packaged {name}:"), line.trim_end().to_owned()]
        );
    }
}

/// Asserts that a package whose `kbf` is not the bytes of the attested tarball's, by one
/// byte, is refused with the digests of both, and that a tarball with no `kbf` is named.
#[test]
fn a_package_whose_kbf_is_not_the_attested_one_is_refused() {
    let mut fix = Fix::new();
    let mut other = fix.kbf.clone();
    *other.last_mut().expect("byte") ^= 1;
    let good = fix.kbf.clone();
    fix.kbf = other;
    let got = found("kbf-differs", &fix);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("usr/bin/kbf (sha256:"), "{got:?}");
    assert!(got[0].contains("is not the binary of"), "{got:?}");
    let in_deb = Sha256::of(&fix.kbf).to_string();
    let in_tarball = Sha256::of(&good).to_string();
    assert!(got[0].contains(&in_deb), "{got:?}");
    assert!(got[0].contains(&in_tarball), "{got:?}");
    let fix = Fix::new();
    let (dir, deb, _) = world("other-name", &fix, &fix.kbf, Platform::LinuxX86_64);
    let tarball = dir.join("t.tar.gz");
    let wrong = binary_tarball("kbf-installer", &fix.kbf).expect("tarball");
    std::fs::write(&tarball, wrong).expect("t");
    let c = check(Platform::LinuxX86_64, &tarball);
    let got = problems(&std::fs::read(&deb).expect("deb"), &c).expect("problems");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].ends_with("holds no kbf"), "{got:?}");
}

/// Asserts that a package whose `kbf` is not mode 0755 and one whose units are not mode 0644
/// are refused, each naming the file and the mode.
#[test]
fn the_kbf_must_be_executable_and_the_units_plain_files() {
    let mut fix = Fix::new();
    fix.kbf_mode = 0o700;
    let got = found("kbf-mode", &fix);
    assert_eq!(got, ["usr/bin/kbf has mode 0700, not 0755"]);
    let mut fix = Fix::new();
    fix.unit_mode = 0o755;
    let got = found("unit-mode", &fix);
    assert_eq!(got.len(), KBF_UNITS.len(), "{got:?}");
    for unit in KBF_UNITS {
        let want = format!("{} has mode 0755, not 0644", in_units(unit));
        assert!(got.contains(&want), "{want} in {got:?}");
    }
}

/// Asserts that a package missing `usr/bin/kbf`, or the unit of any one mode, is refused
/// naming the missing file and nothing else.
#[test]
fn a_package_without_the_kbf_or_the_unit_of_a_mode_is_refused() {
    let mut paths = vec!["usr/bin/kbf".to_owned()];
    for unit in KBF_UNITS {
        paths.push(in_units(unit));
    }
    for gone in paths {
        let mut fix = Fix::new();
        fix.drop = Some(gone.clone());
        assert_eq!(found("missing", &fix), [format!("holds no {gone}")]);
    }
}

/// Asserts that a package that holds a file of the `kbf-installer` package, or any file but
/// the binary and the units, is refused naming it.
#[test]
fn a_package_that_holds_another_file_is_refused() {
    for path in ["usr/bin/kbf-installer", "usr/lib/systemd/system/kbf.slice"] {
        let mut fix = Fix::new();
        fix.extra.push((path.to_owned(), 0o644, b"x".to_vec()));
        let want = format!("holds {path}, which the kbf package does not ship");
        assert_eq!(found("extra", &fix), [want]);
    }
}

/// Asserts that the control file must name the package `kbf` and the architecture of the
/// platform: another name and another architecture are refused, each alone.
#[test]
fn the_control_file_names_the_package_and_the_architecture() {
    let mut fix = Fix::new();
    fix.package = "kbf-installer";
    let got = found("name", &fix);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("Package \"kbf-installer\", not \"kbf\""),
        "{got:?}"
    );
    let mut fix = Fix::new();
    fix.arch = "arm64";
    let got = found("arch", &fix);
    let want = "control says Architecture \"arm64\", not \"amd64\" for linux-x86_64";
    assert_eq!(got, [want]);
}

/// Asserts that the control file must depend on `kbf-installer (>= <the package's own
/// version>)`: no dependency, another package, an older or other version and another name
/// are refused, each alone, and the same dependency among others, and spelled with other
/// spaces, is not.
#[test]
fn the_control_file_depends_on_the_installer_of_the_packages_version() {
    let with_depends = |depends: &str| {
        let mut fix = Fix::new();
        fix.depends = depends.to_owned();
        found("depends", &fix)
    };
    let hold = format!("which does not hold \"kbf-installer (>= {VERSION})\"");
    for depends in [
        "",
        "systemd",
        "kbf-installer",
        "kbf-installer (>= 0.0.1+0123456789ab)",
        "kbf-installer (= 0.1.0+0123456789ab)",
        "kbf-installer-extra (>= 0.1.0+0123456789ab)",
        "kbf (>= 0.1.0+0123456789ab)",
    ] {
        let got = with_depends(depends);
        assert_eq!(got.len(), 1, "{depends:?}: {got:?}");
        assert!(got[0].contains(&hold), "{depends:?}: {got:?}");
    }
    for depends in [
        "kbf-installer(>=0.1.0+0123456789ab)",
        "systemd, kbf-installer (>= 0.1.0+0123456789ab), crun",
        "kbf-installer  ( >=  0.1.0+0123456789ab )",
    ] {
        let got = with_depends(depends);
        assert_eq!(got, Vec::<String>::new(), "{depends:?}");
    }
    let mut fix = Fix::new();
    fix.version = "0.2.0+fedcba987654";
    let got = found("version", &fix);
    assert_eq!(got.len(), 1, "{got:?}");
    let want = "which does not hold \"kbf-installer (>= 0.2.0+fedcba987654)\"";
    assert!(got[0].contains(want), "{got:?}");
}

/// Asserts that the control file must say that the package replaces `kbf-installer`, of any
/// version, because the units of the modes were that package's files: no `Replaces`, another
/// package, a name that only starts like it and a replacement of some versions only are
/// refused, each alone, and the name among others, or with spaces around it, is not.
#[test]
fn the_control_file_replaces_the_installer() {
    let with_replaces = |replaces: &str| {
        let mut fix = Fix::new();
        fix.replaces = replaces.to_owned();
        found("replaces", &fix)
    };
    let hold = "which does not hold \"kbf-installer\"";
    for replaces in ["", "kbf", "kbf-installer-extra", "kbf-installer (<< 9)"] {
        let got = with_replaces(replaces);
        assert_eq!(got.len(), 1, "{replaces:?}: {got:?}");
        assert!(got[0].starts_with("control says Replaces "), "{got:?}");
        assert!(got[0].contains(hold), "{replaces:?}: {got:?}");
    }
    for replaces in ["kbf-installer", "other, kbf-installer", "kbf-installer  "] {
        let got = with_replaces(replaces);
        assert_eq!(got, Vec::<String>::new(), "{replaces:?}");
    }
}

/// Asserts that a control file without the `Package` line, and one without the `Depends` and
/// `Replaces` lines, are refused, the missing `Package` shown as `(none)` and the missing
/// others as empty values.
#[test]
fn a_control_file_without_its_lines_is_refused() {
    let fix = Fix::new();
    let (_, _, tarball) = world("bare-control", &fix, &fix.kbf, Platform::LinuxX86_64);
    let control = tgz(&[("control", 0o644, b"Architecture: amd64\n")]);
    let data = tgz(&[("usr/bin/kbf", 0o755, fix.kbf.as_slice())]);
    let deb = ar(&[("control.tar.gz", &control), ("data.tar.gz", &data)]);
    let c = check(Platform::LinuxX86_64, &tarball);
    let got = problems(&deb, &c).expect("problems");
    let package = "control says Package \"(none)\", not \"kbf\"".to_owned();
    assert!(got.contains(&package), "{got:?}");
    let depends = got
        .iter()
        .any(|p| p.starts_with("control says Depends \"\""));
    assert!(depends, "{got:?}");
    let replaces = got
        .iter()
        .any(|p| p.starts_with("control says Replaces \"\""));
    assert!(replaces, "{got:?}");
}

/// Asserts that a damaged archive is named and not taken for a package: bytes that are no
/// `ar` give that one problem, and a deb without its control tarball says so.
#[test]
fn a_damaged_archive_is_named_not_taken_for_a_package() {
    let fix = Fix::new();
    let (_, _, tarball) = world("damaged", &fix, &fix.kbf, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball);
    let got = problems(b"hello", &c).expect("problems");
    let want = "not an ar archive (a deb starts `!<arch>`)";
    assert_eq!(got, [want]);
    let no_data = ar(&[("debian-binary", b"2.0\n")]);
    let got = problems(&no_data, &c).expect("problems");
    let want = "has no control.tar.gz (nfpm is configured for gzip)";
    assert_eq!(got, [want]);
}

/// Asserts that the asset is listed once and written once: a second run with the sums file
/// already holding the name is refused and leaves the sums as they were, and the lines of
/// other assets survive.
#[test]
fn the_asset_is_listed_once_and_written_once() {
    let fix = Fix::new();
    let (dir, deb, tarball) = world("twice", &fix, &fix.kbf, Platform::LinuxX86_64);
    let out = dir.join("dist");
    std::fs::create_dir(&out).expect("dist");
    let earlier = format!(
        "{}  kbf-installer-{COMMIT}-linux-x86_64.deb\n",
        Sha256::of(b"t")
    );
    std::fs::write(out.join("SHA256SUMS.linux-x86_64"), &earlier).expect("sums");
    let c = check(Platform::LinuxX86_64, &tarball);
    asset(&deb, COMMIT, &Mode::Add(&out), &c).expect("first");
    let text = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    assert!(text.starts_with(&earlier), "{text}");
    assert_eq!(text.lines().count(), 2, "{text}");
    let err = asset(&deb, COMMIT, &Mode::Add(&out), &c).expect_err("twice");
    let said = err.to_string();
    let want = format!("already lists kbf-{COMMIT}-linux-x86_64.deb");
    assert!(said.ends_with(&want), "{said}");
    let after = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    assert_eq!(after, text);
}

/// Asserts that a refused package leaves nothing in the output directory: not the asset,
/// not the directory.
#[test]
fn a_refused_package_leaves_nothing_in_the_output() {
    let mut fix = Fix::new();
    fix.package = "other";
    let (dir, deb, tarball) = world("refused", &fix, &fix.kbf, Platform::LinuxX86_64);
    let out = dir.join("dist");
    let c = check(Platform::LinuxX86_64, &tarball);
    let err = asset(&deb, COMMIT, &Mode::Add(&out), &c).expect_err("refused");
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(!out.exists(), "created {}", out.display());
}

/// Asserts that `--verify-only` checks the file by its name as the asset, writes nothing,
/// and refuses what the build side refuses.
#[test]
fn verify_only_checks_the_asset_by_its_name_and_writes_nothing() {
    let fix = Fix::new();
    let (dir, deb, tarball) = world("verify-only", &fix, &fix.kbf, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball);
    let err = asset(&deb, COMMIT, &Mode::VerifyOnly, &c).expect_err("not named as the asset");
    let said = err.to_string();
    let want = format!("is not named kbf-{COMMIT}-linux-x86_64.deb");
    assert!(said.contains(&want), "{said}");
    let named = dir.join(format!("kbf-{COMMIT}-linux-x86_64.deb"));
    std::fs::copy(&deb, &named).expect("copy");
    let before = std::fs::read_dir(&dir).expect("ls").count();
    let printed = asset(&named, COMMIT, &Mode::VerifyOnly, &c).expect("verify");
    assert_eq!(printed.len(), 1, "{printed:?}");
    let start = format!("kbf-{COMMIT}-linux-x86_64.deb: ");
    assert!(printed[0].starts_with(&start), "{printed:?}");
    assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), before);
    let mut bad = Fix::new();
    bad.depends = String::new();
    std::fs::write(&named, bad.deb()).expect("bad deb");
    let err = asset(&named, COMMIT, &Mode::VerifyOnly, &c).expect_err("refused");
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
}

/// Asserts that a commit that is not one and a platform that is not Linux are usage errors,
/// that a tarball that is not there is a failure of the job and not a refusal of the package,
/// and that the asset is named for the commit and the platform.
#[test]
fn a_commit_that_is_not_one_a_mac_and_an_absent_tarball_are_errors_not_findings() {
    let fix = Fix::new();
    let (dir, deb, tarball) = world("usage", &fix, &fix.kbf, Platform::LinuxX86_64);
    let out = dir.join("dist");
    let c = check(Platform::LinuxX86_64, &tarball);
    assert!(matches!(
        asset(&deb, "XYZ", &Mode::Add(&out), &c),
        Err(Error::NotACommit(_))
    ));
    let mac = check(Platform::DarwinArm64, &tarball);
    assert!(matches!(
        asset(&deb, COMMIT, &Mode::Add(&out), &mac),
        Err(Error::Usage(_))
    ));
    let none = dir.join("none.tar.gz");
    let absent = check(Platform::LinuxX86_64, &none);
    let err = problems(&fix.deb(), &absent).expect_err("no tarball");
    assert!(!matches!(err, Error::Refused(_)), "{err:?}");
    assert_eq!(
        file_name(COMMIT, Platform::LinuxArm64),
        format!("kbf-{COMMIT}-linux-arm64.deb")
    );
}
