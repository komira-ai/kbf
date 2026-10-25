use std::path::PathBuf;

use super::*;

const MAIN: &str = "refs/heads/main";
// A package built on the former development branch, `feature/arch-v2`: kept as the
// non-main ref hosts that installed such a package still trust (docs/artifacts.md).
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

/// What a good package's trust directory holds, each of which a test may change.
#[derive(Clone)]
struct Fix {
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
            "Package: kbf\nVersion: 0.1.0\nArchitecture: {}\n",
            self.arch
        );
        let control_tgz = tgz(&[("control", 0o644, control.as_bytes())]);
        let mut files: Vec<(&str, u32, &[u8])> = vec![
            ("usr/share/kbf/trust/gh", self.gh_mode, &self.gh),
            (
                "usr/share/kbf/trust/installer.conf",
                0o644,
                self.conf.as_bytes(),
            ),
            (
                "usr/share/kbf/trust/trusted_root.jsonl",
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

/// The pinned `gh` of a scratch directory, with the bytes a good package holds.
fn pinned(name: &str) -> PathBuf {
    let gh = scratch(name).join("gh");
    std::fs::write(&gh, Fix::new().gh).expect("gh");
    gh
}

/// What opening `fix` and `trust_problems` say of it as the trust directory of a package
/// attested for `expect`, with the pinned `gh` given or not.
fn trust(fix: &Fix, expect: &str, gh: Option<&Path>) -> Vec<String> {
    let mut problems = Vec::new();
    let opened = open(&fix.deb(), Platform::LinuxX86_64, &mut problems).expect("opens");
    problems.extend(trust_problems(&opened, expect, gh).expect("problems"));
    problems
}

/// The problems of `fix` as the package for `expect` on x86_64, with the pinned `gh`.
fn found(name: &str, fix: &Fix, expect: &str) -> Vec<String> {
    trust(fix, expect, Some(&pinned(name)))
}

#[test]
fn a_trust_directory_of_the_pinned_gh_that_trusts_its_branch_is_accepted() {
    // Control for the checks: catches one that refuses a correct directory for either
    // branch, or a pinned gh given and the same bytes.
    for r in [MAIN, FEATURE] {
        let mut fix = Fix::new();
        fix.conf = format!("SOURCE_REFS={r}\n");
        assert_eq!(found("good", &fix, r), Vec::<String>::new(), "{r}");
    }
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
    // (a wrong-architecture or substituted one), an empty gh, a gh that is not 0755, and,
    // with no pinned gh given (verify-only), the byte comparison alone being skipped and
    // not the rest.
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
    fix.gh = b"\x7fELF another gh".to_vec();
    assert_eq!(trust(&fix, MAIN, None), Vec::<String>::new());
}

#[test]
fn a_missing_trust_file_and_a_bad_root_are_refused() {
    // Catches: a trust file missing from the package, and a trusted root that is not JSON
    // lines.
    for gone in [
        "usr/share/kbf/trust/gh",
        "usr/share/kbf/trust/installer.conf",
        "usr/share/kbf/trust/trusted_root.jsonl",
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
}

#[test]
fn a_pinned_gh_that_cannot_be_read_is_an_error_not_a_finding() {
    // Catches: the job's own input being absent taken for a refusal of the package.
    let none = scratch("absent-gh").join("none");
    let mut problems = Vec::new();
    let opened = open(&Fix::new().deb(), Platform::LinuxX86_64, &mut problems).expect("opens");
    let err = trust_problems(&opened, MAIN, Some(&none)).expect_err("no gh");
    assert!(!matches!(err, Error::Refused(_)), "{err:?}");
}

#[test]
fn the_wrong_architecture_is_refused() {
    // Catches: an arm64 package shipped under the x86_64 asset name (or the reverse).
    let mut fix = Fix::new();
    fix.arch = "arm64";
    let mut problems = Vec::new();
    let _ = open(&fix.deb(), Platform::LinuxX86_64, &mut problems);
    assert_eq!(
        problems,
        ["control says Architecture \"arm64\", not \"amd64\" for linux-x86_64"]
    );
    let mut problems = Vec::new();
    let _ = open(&fix.deb(), Platform::LinuxArm64, &mut problems);
    assert_eq!(problems, Vec::<String>::new());
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
fn the_version_is_what_the_program_says_after_its_name_and_nothing_else() {
    // Catches: a version taken from another program's output (the test stub's, another
    // binary's), from a binary that failed, from one that printed more or fewer words,
    // or one with no commit (`+`), which would name a package that no commit names.
    assert_eq!(
        version_of(true, "kbf 0.1.0+0123456789ab\n", "kbf").as_deref(),
        Some("0.1.0+0123456789ab")
    );
    for (ok, out) in [
        (false, "kbf 0.1.0+0123456789ab\n"),
        (true, "kbf-installer 0.1.0+0123456789ab\n"),
        (true, "kbf 0.1.0\n"),
        (true, "kbf\n"),
        (true, "kbf 0.1.0+0123456789ab extra\n"),
        (true, "kbf-installer-stub (test only) --version\n"),
        (true, ""),
    ] {
        assert_eq!(version_of(ok, out, "kbf"), None, "{ok} {out:?}");
    }
}

/// An `ar` header of 60 bytes with `size` as its size field and `end` as its terminator.
fn raw_ar(size: &str, end: &str) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    out.extend(
        format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}{end}",
            "x/", 0, 0, 0, "100644", size
        )
        .as_bytes(),
    );
    out
}

/// The problems of opening `deb` as the package for x86_64.
fn problems_of(deb: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let _ = open(deb, Platform::LinuxX86_64, &mut problems);
    problems
}

#[test]
fn a_damaged_archive_is_named_not_taken_for_a_package() {
    // Catches: an `ar` header without its terminator or with no size read as a member
    // (or a panic), a control or data tarball that is not gzip or not a tar, a deb with
    // no data tarball, and a tar cut short inside a file, each accepted or reported as
    // something else than what is wrong with the package.
    let bad = raw_ar("10", "XX");
    assert_eq!(
        problems_of(&bad),
        ["an ar member header without its terminator"]
    );
    let bad = raw_ar("abc", "`\n");
    assert_eq!(problems_of(&bad), ["x/: an ar member with no size"]);
    let good = tgz(&[("control", 0o644, b"Architecture: amd64\n")]);
    let junk_gz = ar(&[("control.tar.gz", b"not gzip"), ("data.tar.gz", &good)]);
    assert!(
        problems_of(&junk_gz)[0].starts_with("control.tar.gz: not a gzip stream"),
        "{:?}",
        problems_of(&junk_gz)
    );
    let no_data = ar(&[("control.tar.gz", &good)]);
    assert_eq!(
        problems_of(&no_data),
        ["has no data.tar.gz (nfpm is configured for gzip)"]
    );
    let junk_data = ar(&[("control.tar.gz", &good), ("data.tar.gz", b"not gzip")]);
    let got = problems_of(&junk_data);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("data.tar.gz: not a gzip stream"),
        "{got:?}"
    );
    // A gzip stream of bytes that are no tar header.
    let not_tar = {
        use std::io::Write;
        let mut gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
        gz.write_all(&[7u8; 1024]).expect("write");
        gz.finish().expect("gzip")
    };
    let got = problems_of(&ar(&[
        ("control.tar.gz", &not_tar),
        ("data.tar.gz", &not_tar),
    ]));
    assert!(got[0].starts_with("control.tar.gz: not a tar"), "{got:?}");
    assert!(
        got.iter().any(|p| p.starts_with("data.tar.gz: not a tar")),
        "{got:?}"
    );
    // A file whose header says a mode that is no number, and one cut short.
    let tar_of = |bad_mode: bool, body: &[u8], keep: usize| {
        use std::io::Write;
        let mut h = tar::Header::new_ustar();
        h.set_path("./usr/bin/kbf-installer").expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(body.len() as u64);
        h.set_mode(0o755);
        if bad_mode {
            h.as_old_mut().mode = *b"zzzzzzz\0";
        }
        h.set_cksum();
        let mut tar = tar::Builder::new(Vec::new());
        tar.append(&h, body).expect("append");
        let mut raw = tar.into_inner().expect("tar");
        raw.truncate(keep);
        let mut gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).expect("write");
        gz.finish().expect("gzip")
    };
    let got = problems_of(&ar(&[
        ("control.tar.gz", &good),
        ("data.tar.gz", &tar_of(true, b"x", 1024)),
    ]));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("data.tar.gz: usr/bin/kbf-installer: "),
        "{got:?}"
    );
    let got = problems_of(&ar(&[
        ("control.tar.gz", &good),
        ("data.tar.gz", &tar_of(false, &[1u8; 1000], 522)),
    ]));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("data.tar.gz: "), "{got:?}");
}

#[test]
fn directory_entries_of_the_data_tarball_are_not_files_of_the_package() {
    // Catches: nfpm's `./usr/` directory entries taken for files (a package that holds
    // everything refused), or the files after them lost.
    let mut fix = Fix::new();
    fix.dirs = true;
    let mut problems = Vec::new();
    let opened = open(&fix.deb(), Platform::LinuxX86_64, &mut problems).expect("opens");
    assert_eq!(problems, Vec::<String>::new());
    let paths: Vec<&str> = opened.files.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "usr/share/kbf/trust/gh",
            "usr/share/kbf/trust/installer.conf",
            "usr/share/kbf/trust/trusted_root.jsonl"
        ]
    );
}

#[test]
fn a_program_that_does_not_run_and_a_truncated_header_are_errors_not_findings() {
    // Catches: a binary that does not run, an empty directory and a header cut short taken
    // for a refusal of the package (or a panic).
    let dir = scratch("absent");
    assert!(binary_version(&dir.join("none")).is_err());
    let empty = dir.join("empty");
    std::fs::create_dir(&empty).expect("dir");
    assert!(sole(&empty, "deb").is_err());
    // A header cut short: 8 bytes of magic and 10 of a member header.
    let got = problems_of(&[&b"!<arch>\n"[..], &[b'x'; 10]].concat());
    assert_eq!(got, ["a truncated ar member header"]);
}
