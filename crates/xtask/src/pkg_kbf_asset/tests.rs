use std::path::PathBuf;

use super::*;
use crate::fetch::Sha256;
use crate::package::tarball as binary_tarball;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.1.0+0123456789ab";
const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature/arch-v2";
const ROOT: &str = "{\"mediaType\":\"trustedroot\"}\n";
/// The `Recommends` a good deb has: [`DEB_RECOMMENDS`], spelled out so that a change to the list
/// shows here too.
const RECOMMENDS: &str = "crun, uidmap, systemd, ca-certificates";
const REPLACES: &str = "kbf-installer (<< 0.1.1)";

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
    /// The mode of every file but the binary and `gh`.
    unit_mode: u32,
    gh: Vec<u8>,
    conf: String,
    package: &'static str,
    version: &'static str,
    arch: &'static str,
    /// Control lines of a relation the good deb has none of, `Depends` for one.
    relations: Vec<(&'static str, String)>,
    recommends: String,
    suggests: String,
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
            gh: b"\x7fELF the pinned gh".to_vec(),
            conf: format!("SOURCE_REFS={MAIN}\n"),
            package: "kbf",
            version: VERSION,
            arch: "amd64",
            relations: Vec::new(),
            recommends: RECOMMENDS.to_owned(),
            suggests: "postgresql".to_owned(),
            replaces: REPLACES.to_owned(),
            drop: None,
            extra: Vec::new(),
        }
    }

    /// The deb: nfpm's layout, gzip members, `./` paths. The control file has a line only for
    /// a relation the fixture has.
    fn deb(&self) -> Vec<u8> {
        let line = |name: &str, value: &str| {
            if value.is_empty() {
                String::new()
            } else {
                format!("{name}: {value}\n")
            }
        };
        let control = format!(
            "Package: {}\nVersion: {}\nArchitecture: {}\n{}{}{}{}",
            self.package,
            self.version,
            self.arch,
            line("Recommends", &self.recommends),
            line("Suggests", &self.suggests),
            line("Replaces", &self.replaces),
            self.relations
                .iter()
                .map(|(name, value)| line(name, value))
                .collect::<String>(),
        );
        let control_tgz = tgz(&[("control", 0o644, control.as_bytes())]);
        let binary = "usr/bin/kbf".to_owned();
        let mut files = vec![(binary, self.kbf_mode, self.kbf.as_slice())];
        for unit in KBF_UNITS.into_iter().chain([SLICE]) {
            files.push((in_units(unit), self.unit_mode, unit.as_bytes()));
        }
        for (path, text) in [
            ("usr/lib/sysusers.d/kbf.conf", "u kbf"),
            ("usr/lib/tmpfiles.d/kbf.conf", "d /x"),
            ("etc/needrestart/conf.d/kbf.conf", "# n"),
        ] {
            files.push((path.to_owned(), self.unit_mode, text.as_bytes()));
        }
        files.push((
            "usr/share/kbf/trust/gh".to_owned(),
            0o755,
            self.gh.as_slice(),
        ));
        files.push((
            "usr/share/kbf/trust/installer.conf".to_owned(),
            0o644,
            self.conf.as_bytes(),
        ));
        files.push((
            "usr/share/kbf/trust/trusted_root.jsonl".to_owned(),
            0o644,
            ROOT.as_bytes(),
        ));
        files.retain(|(p, _, _)| Some(p) != self.drop.as_ref());
        for (path, mode, bytes) in &self.extra {
            files.push((path.clone(), *mode, bytes.as_slice()));
        }
        let listed: Vec<(&str, u32, &[u8])> =
            files.iter().map(|(p, m, b)| (p.as_str(), *m, *b)).collect();
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

/// The checks for a package attested on `main`, with no pinned `gh` to compare.
fn check(platform: Platform, tarball: &Path) -> Check<'_> {
    Check {
        platform,
        kbf_tarball: tarball,
        expect_ref: MAIN,
        gh: None,
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

/// Asserts that a package of the attested `kbf`, depending on no package and replacing the
/// installer, with the unit of every mode, is accepted as the asset for either platform: copied
/// under a name of the commit and platform, with its digest in the sums file.
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

/// Asserts that a package whose `kbf` is not mode 0755 and one whose other files are not mode
/// 0644 are refused, each naming the file and the mode.
#[test]
fn the_kbf_must_be_executable_and_the_other_files_plain() {
    let mut fix = Fix::new();
    fix.kbf_mode = 0o700;
    let got = found("kbf-mode", &fix);
    assert_eq!(got, ["usr/bin/kbf has mode 0700, not 0755"]);
    let mut fix = Fix::new();
    fix.unit_mode = 0o755;
    let got = found("unit-mode", &fix);
    let mut want: Vec<String> = KBF_UNITS
        .into_iter()
        .chain([SLICE])
        .map(in_units)
        .chain(
            [
                "usr/lib/sysusers.d/kbf.conf",
                "usr/lib/tmpfiles.d/kbf.conf",
                "etc/needrestart/conf.d/kbf.conf",
            ]
            .map(str::to_owned),
        )
        .map(|path| format!("{path} has mode 0755, not 0644"))
        .collect();
    want.sort();
    let mut got = got;
    got.sort();
    assert_eq!(got, want);
}

/// Asserts that a package missing `usr/bin/kbf`, the unit of any one mode, the slice, the
/// sysusers, tmpfiles or needrestart file, or any trust file, is refused naming the missing
/// file and nothing else.
#[test]
fn a_package_without_any_file_it_ships_is_refused() {
    let mut paths = vec!["usr/bin/kbf".to_owned()];
    for unit in KBF_UNITS.into_iter().chain([SLICE]) {
        paths.push(in_units(unit));
    }
    for path in [
        "usr/lib/sysusers.d/kbf.conf",
        "usr/lib/tmpfiles.d/kbf.conf",
        "etc/needrestart/conf.d/kbf.conf",
        "usr/share/kbf/trust/gh",
        "usr/share/kbf/trust/installer.conf",
        "usr/share/kbf/trust/trusted_root.jsonl",
    ] {
        paths.push(path.to_owned());
    }
    for gone in paths {
        let mut fix = Fix::new();
        fix.drop = Some(gone.clone());
        assert_eq!(found("missing", &fix), [format!("holds no {gone}")]);
    }
}

/// Asserts that a package that holds a file of the retired installer, or any file it does not
/// ship, is refused naming it.
#[test]
fn a_package_that_holds_another_file_is_refused() {
    for path in [
        "usr/bin/kbf-installer",
        "usr/lib/systemd/system/kbf-proxy.service",
        "usr/lib/systemd/system/kbf-installer.service",
        "usr/share/kbf-installer/trust/gh",
        "usr/share/kbf/trust/extra",
    ] {
        let mut fix = Fix::new();
        fix.extra.push((path.to_owned(), 0o644, b"x".to_vec()));
        let want = format!("holds {path}, which the kbf package does not ship");
        assert_eq!(found("extra", &fix), [want]);
    }
}

/// Asserts that the verb holds the trust directory to the branch the package is attested for,
/// in both directions, and to the pinned `gh`: a main package whose conf trusts the other
/// branch, a feature package attested on main, and a `gh` that is not the downloaded one are
/// each refused alone. (`pkg_asset`'s tests hold the checks themselves to their cases.)
#[test]
fn the_trust_directory_is_for_the_branch_and_holds_the_pinned_gh() {
    let mut fix = Fix::new();
    fix.conf = format!("SOURCE_REFS={FEATURE}\n");
    let got = found("trust-feature", &fix);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains(
            "trusts refs/heads/feature/arch-v2, but this package is attested for refs/heads/main"
        ),
        "{got:?}"
    );
    let (dir, deb, tarball) = world("trust-expect", &fix, &fix.kbf, Platform::LinuxX86_64);
    let bytes = std::fs::read(&deb).expect("deb");
    let c = Check {
        expect_ref: FEATURE,
        ..check(Platform::LinuxX86_64, &tarball)
    };
    assert_eq!(
        problems(&bytes, &c).expect("problems"),
        Vec::<String>::new()
    );
    let gh = dir.join("gh");
    std::fs::write(&gh, b"\x7fELF another gh").expect("gh");
    let c = Check {
        expect_ref: FEATURE,
        gh: Some(&gh),
        ..check(Platform::LinuxX86_64, &tarball)
    };
    let got = problems(&bytes, &c).expect("problems");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("is not the pinned gh"), "{got:?}");
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

/// Asserts that the control file has no `Depends`, `Pre-Depends`, `Breaks` or `Conflicts`, each
/// refused alone with whatever it says (the installer's name, another package, or both): a host
/// updates the package with a plain `dpkg --install`, which resolves nothing, so a dependency the
/// host lacks leaves the package unconfigured and a `Breaks` of the installer makes dpkg refuse.
#[test]
fn the_control_file_has_no_dependency_break_or_conflict() {
    for name in ["Depends", "Pre-Depends", "Breaks", "Conflicts"] {
        for value in [
            "crun",
            "kbf-installer",
            "kbf-installer (<< 0.1.1)",
            "systemd, kbf-installer (>= 0.1.0+0123456789ab)",
        ] {
            let mut fix = Fix::new();
            fix.relations.push((name, value.to_owned()));
            let got = found("relations", &fix);
            assert_eq!(got.len(), 1, "{name} {value:?}: {got:?}");
            assert!(
                got[0].starts_with(&format!(
                    "control says {name} {value:?}, but the kbf package has none"
                )),
                "{name} {value:?}: {got:?}"
            );
        }
    }
}

/// Asserts that the control file must recommend what the package wants, the installer's
/// package having been where those came from, and suggest Postgres: each missing one is
/// refused alone, naming it.
#[test]
fn the_control_file_recommends_what_the_package_wants() {
    for gone in ["crun", "uidmap", "systemd", "ca-certificates"] {
        let mut fix = Fix::new();
        fix.recommends = RECOMMENDS
            .split(", ")
            .filter(|d| *d != gone)
            .collect::<Vec<_>>()
            .join(", ");
        let got = found("needs", &fix);
        assert_eq!(got.len(), 1, "{gone}: {got:?}");
        assert!(
            got[0].starts_with("control says Recommends ")
                && got[0].ends_with(&format!("which does not hold \"{gone}\"")),
            "{gone}: {got:?}"
        );
    }
    let mut fix = Fix::new();
    fix.suggests = String::new();
    let got = found("suggests", &fix);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].ends_with("which does not hold \"postgresql\""),
        "{got:?}"
    );
}

/// Asserts that the control file must say that the package replaces the `kbf-installer` below
/// the bound, because the units, the slice, the trust files and the rest were that package's
/// files: no line, another package, a name that only starts like it, the package unbounded and a
/// bound that is not the one are refused, each alone, and the entry among others, or with other
/// spacing, is not.
#[test]
fn the_control_file_replaces_the_installer_below_the_bound() {
    let with = |value: &str| {
        let mut fix = Fix::new();
        fix.replaces = value.to_owned();
        found("relation", &fix)
    };
    let hold = format!("which does not hold \"{REPLACES}\"");
    for value in [
        "",
        "kbf",
        "kbf-installer-extra (<< 0.1.1)",
        "kbf-installer",
        "kbf-installer (<< 9)",
        "kbf-installer (<= 0.1.1)",
    ] {
        let got = with(value);
        assert_eq!(got.len(), 1, "{value:?}: {got:?}");
        assert!(got[0].starts_with("control says Replaces "), "{got:?}");
        assert!(got[0].ends_with(&hold), "{value:?}: {got:?}");
    }
    for value in [
        REPLACES,
        "other, kbf-installer (<< 0.1.1)",
        "kbf-installer(<<0.1.1)  ",
    ] {
        assert_eq!(with(value), Vec::<String>::new(), "{value:?}");
    }
}

/// Asserts that a control file without the `Package` line, and one without the relations, are
/// refused, the missing `Package` shown as `(none)` and each missing relation as an empty
/// value.
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
    for field in ["Recommends", "Suggests", "Replaces"] {
        let said = got
            .iter()
            .any(|p| p.starts_with(&format!("control says {field} \"\"")));
        assert!(said, "{field}: {got:?}");
    }
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
    let earlier = format!("{}  kbf-{COMMIT}-linux-x86_64.tar.gz\n", Sha256::of(b"t"));
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
    bad.relations.push(("Depends", "crun".to_owned()));
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
