use std::path::PathBuf;

use super::*;
use crate::package::tarball as binary_tarball;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature/arch-v2";
const ROOT: &str = "{\"mediaType\":\"trustedroot\"}\n";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-asset-unit")
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
        h.set_entry_type(if path.ends_with('/') {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
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
        assert_eq!(header.len(), 60);
        out.extend(header.as_bytes());
        out.extend(*body);
        if body.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// What a good package holds, each of which a test may change.
#[derive(Clone)]
struct Fix {
    binary: Vec<u8>,
    binary_mode: u32,
    gh: Vec<u8>,
    gh_mode: u32,
    conf: String,
    root: String,
    arch: &'static str,
    drop: Option<&'static str>,
    /// A directory entry ahead of the files, as nfpm writes `./usr/`.
    dirs: bool,
}

impl Fix {
    fn new() -> Fix {
        Fix {
            binary: b"\x7fELF the kbf-installer built for the commit".to_vec(),
            binary_mode: 0o755,
            gh: b"\x7fELF the pinned gh".to_vec(),
            gh_mode: 0o755,
            conf: format!("SOURCE_REFS={MAIN}\n"),
            root: ROOT.to_owned(),
            arch: "amd64",
            drop: None,
            dirs: false,
        }
    }

    /// The deb: nfpm's layout, gzip members, `./` paths.
    fn deb(&self) -> Vec<u8> {
        let control = format!(
            "Package: kbf-installer\nVersion: 0.1.0\nArchitecture: {}\n",
            self.arch
        );
        let control_tgz = tgz(&[("control", 0o644, control.as_bytes())]);
        let mut files: Vec<(&str, u32, &[u8])> = vec![
            ("usr/bin/kbf-installer", self.binary_mode, &self.binary),
            ("usr/share/kbf-installer/trust/gh", self.gh_mode, &self.gh),
            (
                "usr/share/kbf-installer/trust/installer.conf",
                0o644,
                self.conf.as_bytes(),
            ),
            (
                "usr/share/kbf-installer/trust/trusted_root.jsonl",
                0o644,
                self.root.as_bytes(),
            ),
        ];
        files.retain(|(p, _, _)| Some(*p) != self.drop);
        if self.dirs {
            files.insert(0, ("usr/", 0o755, &[]));
        }
        ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", &control_tgz),
            ("data.tar.gz", &tgz(&files)),
        ])
    }
}

/// A directory with the attested tarball of `binary` for `platform`, the pinned gh and
/// `deb`: (dir, deb path, tarball path, gh path).
fn world(
    name: &str,
    fix: &Fix,
    binary: &[u8],
    platform: Platform,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = scratch(name);
    let tarball = dir.join(format!("kbf-installer-{COMMIT}-{}.tar.gz", platform.name()));
    std::fs::write(
        &tarball,
        binary_tarball("kbf-installer", binary).expect("tarball"),
    )
    .expect("tarball");
    let gh = dir.join("gh");
    std::fs::write(&gh, b"\x7fELF the pinned gh").expect("gh");
    let deb = dir.join("kbf-installer_0.1.0+0123456789ab_amd64.deb");
    std::fs::write(&deb, fix.deb()).expect("deb");
    (dir, deb, tarball, gh)
}

fn check<'a>(platform: Platform, tarball: &'a Path, r: &'a str, gh: Option<&'a Path>) -> Check<'a> {
    Check {
        platform,
        binary_tarball: tarball,
        expect_ref: r,
        gh,
    }
}

/// The problems of `fix` as the package for `expect` on x86_64.
fn found(name: &str, fix: &Fix, expect: &str) -> Vec<String> {
    let (_, deb, tarball, gh) = world(name, fix, &Fix::new().binary, Platform::LinuxX86_64);
    problems(
        &std::fs::read(&deb).expect("deb"),
        &check(Platform::LinuxX86_64, &tarball, expect, Some(&gh)),
    )
    .expect("problems")
}

#[test]
fn a_package_of_the_attested_binary_that_trusts_its_branch_is_accepted_as_the_asset() {
    // Control for the whole verb: catches a check that refuses a correct package for
    // either platform or either branch, an asset not named for the commit and platform,
    // a sums line that is not the file's digest, and an asset that is not the package's
    // bytes.
    for (platform, arch, r) in [
        (Platform::LinuxX86_64, "amd64", MAIN),
        (Platform::LinuxArm64, "arm64", FEATURE),
    ] {
        let mut fix = Fix::new();
        fix.arch = arch;
        fix.conf = format!("SOURCE_REFS={r}\n");
        let (dir, deb, tarball, gh) = world("good", &fix, &fix.binary, platform);
        let out = dir.join("dist");
        let printed = asset(
            &deb,
            COMMIT,
            &Mode::Add(&out),
            &check(platform, &tarball, r, Some(&gh)),
        )
        .expect("asset");
        let name = format!("kbf-installer-{COMMIT}-{}.deb", platform.name());
        let bytes = std::fs::read(&deb).expect("deb");
        assert_eq!(std::fs::read(out.join(&name)).expect("asset"), bytes);
        let line = format!("{}  {name}\n", Sha256::of(&bytes));
        assert_eq!(
            std::fs::read_to_string(out.join(format!("SHA256SUMS.{}", platform.name())))
                .expect("sums"),
            line
        );
        assert_eq!(
            printed,
            vec![format!("packaged {name}:"), line.trim_end().to_owned()]
        );
    }
}

#[test]
fn a_package_whose_binary_is_not_the_attested_one_is_refused() {
    // Catches: the deb carrying a binary other than the one `build-commit check-asset`
    // showed names the commit and the attestation covers (a one-byte difference is
    // enough), and a tarball with no kbf-installer in it.
    let mut fix = Fix::new();
    let mut other = fix.binary.clone();
    *other.last_mut().expect("byte") ^= 1;
    fix.binary = other;
    let got = found("binary-differs", &fix, MAIN);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("usr/bin/kbf-installer (sha256:")
            && got[0].contains("is not the binary of"),
        "{got:?}"
    );
    // The tarball holds another file name.
    let fix = Fix::new();
    let (dir, deb, _, gh) = world(
        "tarball-other-name",
        &fix,
        &fix.binary,
        Platform::LinuxX86_64,
    );
    let tarball = dir.join("t.tar.gz");
    std::fs::write(
        &tarball,
        binary_tarball("kbf", &fix.binary).expect("tarball"),
    )
    .expect("t");
    let got = problems(
        &std::fs::read(&deb).expect("deb"),
        &check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh)),
    )
    .expect("problems");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].ends_with("holds no kbf-installer"), "{got:?}");
}

#[test]
fn a_package_trusts_exactly_the_branch_it_is_attested_for() {
    // Catches: SOURCE_REFS not compared, or compared loosely, in either direction: a
    // package attested on main whose conf trusts the development branch (a host would
    // accept releases of a branch that is not the one it was signed on: the case that
    // matters), the reverse, a conf whose ref merely contains the expected one
    // (`main` against `main2`), a conf with a second key, and an empty conf.
    let cases = [
        (
            FEATURE,
            MAIN,
            "trusts refs/heads/feature/arch-v2, but this package is attested for refs/heads/main",
        ),
        (
            MAIN,
            FEATURE,
            "trusts refs/heads/main, but this package is attested for refs/heads/feature/arch-v2",
        ),
        (
            "refs/heads/main2",
            MAIN,
            "trusts refs/heads/main2, but this package is attested for refs/heads/main",
        ),
        ("refs/heads/ma", MAIN, "trusts refs/heads/ma, but"),
    ];
    for (conf, expect, want) in cases {
        let mut fix = Fix::new();
        fix.conf = format!("SOURCE_REFS={conf}\n");
        let got = found("conf-mismatch", &fix, expect);
        assert_eq!(got.len(), 1, "{conf} vs {expect}: {got:?}");
        assert!(got[0].contains(want), "{got:?}");
    }
    for conf in [
        "SOURCE_REFS=refs/heads/main\nRUN_AS_UID=0\n",
        "",
        "SOURCE_REFS=refs/heads/main",
    ] {
        let mut fix = Fix::new();
        fix.conf = conf.to_owned();
        let got = found("conf-shape", &fix, MAIN);
        assert_eq!(got.len(), 1, "{conf:?}: {got:?}");
        assert!(
            got[0].contains("is not exactly `SOURCE_REFS=refs/heads/<branch>` and a newline"),
            "{got:?}"
        );
    }
    let got = found("expect-not-a-ref", &Fix::new(), "main");
    assert!(
        got.iter()
            .any(|p| p == "--expect-ref \"main\" is not refs/heads/<branch>"),
        "{got:?}"
    );
}

#[test]
fn the_packaged_gh_is_the_pinned_one_and_executable() {
    // Catches: a gh that is not the program the job downloaded and checked by SHA-256
    // (a wrong-architecture or substituted one), an empty gh, a gh that is not 0755, a
    // binary that is not 0755, and, with no pinned gh given (verify-only), the byte
    // comparison alone being skipped and not the rest.
    let mut fix = Fix::new();
    fix.gh = b"\x7fELF another gh".to_vec();
    let got = found("gh-other", &fix, MAIN);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("is not the pinned gh"), "{got:?}");
    let mut fix = Fix::new();
    fix.gh = Vec::new();
    assert!(
        found("gh-empty", &fix, MAIN)
            .iter()
            .any(|p| p.ends_with("trust/gh is empty"))
    );
    let mut fix = Fix::new();
    fix.gh_mode = 0o644;
    let got = found("gh-mode", &fix, MAIN);
    assert!(
        got.iter()
            .any(|p| p.ends_with("trust/gh has mode 0644, not 0755")),
        "{got:?}"
    );
    let mut fix = Fix::new();
    fix.binary_mode = 0o700;
    assert!(
        found("bin-mode", &fix, MAIN)
            .iter()
            .any(|p| p.ends_with("has mode 0700, not 0755"))
    );
    let mut fix = Fix::new();
    fix.gh = b"\x7fELF another gh".to_vec();
    let (_, deb, tarball, _) = world("gh-unpinned", &fix, &fix.binary, Platform::LinuxX86_64);
    let got = problems(
        &std::fs::read(&deb).expect("deb"),
        &check(Platform::LinuxX86_64, &tarball, MAIN, None),
    )
    .expect("problems");
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn the_wrong_architecture_a_missing_trust_file_and_a_bad_root_are_refused() {
    // Catches: an arm64 package shipped under the x86_64 asset name (or the reverse), a
    // trust file missing from the package, a trusted root that is not JSON lines, and
    // garbage or a cut-short archive taken for a deb.
    let mut fix = Fix::new();
    fix.arch = "arm64";
    let got = found("arch", &fix, MAIN);
    assert_eq!(
        got,
        ["control says Architecture \"arm64\", not \"amd64\" for linux-x86_64"]
    );
    for gone in [
        "usr/bin/kbf-installer",
        "usr/share/kbf-installer/trust/gh",
        "usr/share/kbf-installer/trust/installer.conf",
        "usr/share/kbf-installer/trust/trusted_root.jsonl",
    ] {
        let mut fix = Fix::new();
        fix.drop = Some(gone);
        let got = found("missing", &fix, MAIN);
        assert!(
            got.iter().any(|p| p == &format!("holds no {gone}")),
            "{gone}: {got:?}"
        );
    }
    let mut fix = Fix::new();
    fix.root = "not json\n".to_owned();
    let got = found("bad-root", &fix, MAIN);
    assert!(
        got.iter()
            .any(|p| p.contains("trusted_root.jsonl line 1: not JSON")),
        "{got:?}"
    );
    let (_, _, tarball, gh) = world(
        "garbage",
        &Fix::new(),
        &Fix::new().binary,
        Platform::LinuxX86_64,
    );
    let c = check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh));
    let good = Fix::new().deb();
    assert_eq!(
        problems(b"hello", &c).expect("p"),
        ["not an ar archive (a deb starts `!<arch>`)"]
    );
    assert!(problems(&good[..good.len() - 5], &c).expect("p")[0].contains("cut short"));
    let no_data = ar(&[("debian-binary", b"2.0\n")]);
    assert_eq!(
        problems(&no_data, &c).expect("p"),
        ["has no control.tar.gz (nfpm is configured for gzip)"]
    );
}

#[test]
fn the_asset_is_listed_once_and_written_once() {
    // Catches: the line not appended (the package would be an unlisted file that
    // verify-assets refuses, or worse missing from the attested SHA256SUMS), an existing
    // line for the asset not refused (a second run, or two platforms' jobs sharing a
    // directory, would list it twice), the other lines of the sums file lost, and an
    // asset overwritten.
    let fix = Fix::new();
    let (dir, deb, tarball, gh) = world("twice", &fix, &fix.binary, Platform::LinuxX86_64);
    let out = dir.join("dist");
    std::fs::create_dir(&out).expect("dist");
    let earlier = format!(
        "{}  kbf-installer-{COMMIT}-linux-x86_64.tar.gz\n",
        Sha256::of(b"t")
    );
    std::fs::write(out.join("SHA256SUMS.linux-x86_64"), &earlier).expect("sums");
    let c = check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh));
    asset(&deb, COMMIT, &Mode::Add(&out), &c).expect("first");
    let text = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    assert!(
        text.starts_with(&earlier) && text.lines().count() == 2,
        "{text}"
    );
    let err = asset(&deb, COMMIT, &Mode::Add(&out), &c).expect_err("twice");
    assert!(
        err.to_string().ends_with(&format!(
            "already lists kbf-installer-{COMMIT}-linux-x86_64.deb"
        )),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums"),
        text
    );
}

#[test]
fn a_refused_package_leaves_nothing_in_the_output() {
    // Catches: the asset copied or listed before the checks (an unattested-able file in
    // dist, listed in the sums the attest job signs).
    let mut fix = Fix::new();
    fix.conf = format!("SOURCE_REFS={FEATURE}\n");
    let (dir, deb, tarball, gh) = world("refused", &fix, &fix.binary, Platform::LinuxX86_64);
    let out = dir.join("dist");
    let err = asset(
        &deb,
        COMMIT,
        &Mode::Add(&out),
        &check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh)),
    )
    .expect_err("refused");
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(!out.exists(), "created {}", out.display());
}

#[test]
fn verify_only_checks_the_asset_by_its_name_and_writes_nothing() {
    // Catches: --verify-only on a file that is not named as the asset (the verify job
    // would check some other file than the one signed), writing or listing anything,
    // and the same refusals as the build side (a main package trusting another branch).
    let fix = Fix::new();
    let (dir, deb, tarball, gh) = world("verify-only", &fix, &fix.binary, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh));
    let err = asset(&deb, COMMIT, &Mode::VerifyOnly, &c).expect_err("not named as the asset");
    assert!(
        err.to_string().contains(&format!(
            "is not named kbf-installer-{COMMIT}-linux-x86_64.deb"
        )),
        "{err}"
    );
    let named = dir.join(format!("kbf-installer-{COMMIT}-linux-x86_64.deb"));
    std::fs::copy(&deb, &named).expect("copy");
    let before = std::fs::read_dir(&dir).expect("ls").count();
    let printed = asset(&named, COMMIT, &Mode::VerifyOnly, &c).expect("verify");
    assert_eq!(printed.len(), 1, "{printed:?}");
    assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), before);
    let other = check(Platform::LinuxX86_64, &tarball, FEATURE, Some(&gh));
    assert!(asset(&named, COMMIT, &Mode::VerifyOnly, &other).is_err());
}

#[test]
fn a_commit_that_is_not_one_and_a_mac_are_usage_errors() {
    // Catches: an asset named for a short or upper-case "commit", and a darwin platform
    // (there is no Mac deb).
    let fix = Fix::new();
    let (dir, deb, tarball, gh) = world("usage", &fix, &fix.binary, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh));
    let out = dir.join("dist");
    assert!(matches!(
        asset(&deb, "XYZ", &Mode::Add(&out), &c),
        Err(Error::NotACommit(_))
    ));
    let mac = check(Platform::DarwinArm64, &tarball, MAIN, Some(&gh));
    assert!(matches!(
        asset(&deb, COMMIT, &Mode::Add(&out), &mac),
        Err(Error::Usage(_))
    ));
}

#[test]
fn sole_is_the_file_or_the_one_package_of_the_directory() {
    // Catches: a directory taken as a package, the wrong format picked when nfpm left a
    // deb and an rpm side by side, and two packages or none passing as one (the step would
    // sign and list a deb it did not mean).
    let dir = scratch("sole");
    let file = dir.join("a.deb");
    std::fs::write(&file, b"x").expect("deb");
    assert_eq!(sole(&file, "rpm").expect("a file is itself"), file);
    assert_eq!(sole(&dir, "deb").expect("one deb"), file);
    std::fs::write(dir.join("a.rpm"), b"x").expect("rpm");
    std::fs::create_dir(dir.join("b.deb")).expect("a directory named like a deb");
    assert_eq!(sole(&dir, "deb").expect("the deb of two formats"), file);
    assert_eq!(sole(&dir, "rpm").expect("the rpm"), dir.join("a.rpm"));
    std::fs::write(dir.join("c.deb"), b"x").expect("second deb");
    let err = sole(&dir, "deb").expect_err("two debs");
    assert!(
        err.to_string().starts_with("expected one .deb file in "),
        "{err}"
    );
    let empty = scratch("sole-empty");
    assert!(sole(&empty, "deb").is_err());
}

#[test]
fn the_version_is_what_the_installer_says_after_its_name_and_nothing_else() {
    // Catches: a version taken from another program's output (the test stub's, a
    // `kbf --version`), from a binary that failed, from one that printed more or
    // fewer words, or one with no commit (`+`), which would name a package that no
    // commit names.
    assert_eq!(
        version_of(true, "kbf-installer 0.1.0+0123456789ab\n").as_deref(),
        Some("0.1.0+0123456789ab")
    );
    for (ok, out) in [
        (false, "kbf-installer 0.1.0+0123456789ab\n"),
        (true, "kbf 0.1.0+0123456789ab\n"),
        (true, "kbf-installer 0.1.0\n"),
        (true, "kbf-installer\n"),
        (true, "kbf-installer 0.1.0+0123456789ab extra\n"),
        (true, "kbf-installer-stub (test only) --version\n"),
        (true, ""),
    ] {
        assert_eq!(version_of(ok, out), None, "{ok} {out:?}");
    }
}

/// An `ar` header of 60 bytes with `size` as its size field and `end` as its terminator.
fn raw_ar(size: &str, end: &str) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    out.extend(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}{end}", "x/", 0, 0, 0, "100644", size).as_bytes());
    out
}

/// The problems of `deb` as the package for `MAIN` on x86_64.
fn problems_of(name: &str, deb: &[u8]) -> Vec<String> {
    let (_, _, tarball, gh) = world(name, &Fix::new(), &Fix::new().binary, Platform::LinuxX86_64);
    problems(
        deb,
        &check(Platform::LinuxX86_64, &tarball, MAIN, Some(&gh)),
    )
    .expect("problems")
}

#[test]
fn a_damaged_archive_is_named_not_taken_for_a_package() {
    // Catches: an `ar` header without its terminator or with no size read as a member
    // (or a panic), a control or data tarball that is not gzip or not a tar, a deb with
    // no data tarball, and a tar cut short inside a file, each accepted or reported as
    // something else than what is wrong with the package.
    let bad = raw_ar("10", "XX");
    assert_eq!(
        problems_of("ar-end", &bad),
        ["an ar member header without its terminator"]
    );
    let bad = raw_ar("abc", "`\n");
    assert_eq!(
        problems_of("ar-size", &bad),
        ["x/: an ar member with no size"]
    );
    let good = tgz(&[("control", 0o644, b"Architecture: amd64\n")]);
    let junk_gz = ar(&[("control.tar.gz", b"not gzip"), ("data.tar.gz", &good)]);
    assert!(
        problems_of("junk-control", &junk_gz)[0].starts_with("control.tar.gz: not a gzip stream"),
        "{:?}",
        problems_of("junk-control", &junk_gz)
    );
    let no_data = ar(&[("control.tar.gz", &good)]);
    assert_eq!(
        problems_of("no-data", &no_data),
        ["has no data.tar.gz (nfpm is configured for gzip)"]
    );
    let junk_data = ar(&[("control.tar.gz", &good), ("data.tar.gz", b"not gzip")]);
    let got = problems_of("junk-data", &junk_data);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("data.tar.gz: not a gzip stream"), "{got:?}");
    // A gzip stream of bytes that are no tar header.
    let not_tar = {
        use std::io::Write;
        let mut gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
        gz.write_all(&[7u8; 1024]).expect("write");
        gz.finish().expect("gzip")
    };
    let got = problems_of("not-tar", &ar(&[("control.tar.gz", &not_tar), ("data.tar.gz", &not_tar)]));
    assert!(got[0].starts_with("control.tar.gz: not a tar"), "{got:?}");
    assert!(got.iter().any(|p| p.starts_with("data.tar.gz: not a tar")), "{got:?}");
    // A file whose bytes end before its header says.
    let cut = {
        use std::io::Write;
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_path("./usr/bin/kbf-installer").expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(1000);
        h.set_mode(0o755);
        h.set_cksum();
        tar.append(&h, &[1u8; 1000][..]).expect("append");
        let mut raw = tar.into_inner().expect("tar");
        raw.truncate(512 + 10);
        let mut gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).expect("write");
        gz.finish().expect("gzip")
    };
    let got = problems_of("cut", &ar(&[("control.tar.gz", &good), ("data.tar.gz", &cut)]));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("data.tar.gz: usr/bin/kbf-installer: "), "{got:?}");
}

#[test]
fn directory_entries_of_the_data_tarball_are_not_files_of_the_package() {
    // Catches: nfpm's `./usr/` directory entries taken for files (a package that holds
    // everything refused), or the files after them lost.
    let mut fix = Fix::new();
    fix.dirs = true;
    assert_eq!(found("dirs", &fix, MAIN), Vec::<String>::new());
}
