use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-installer-tarball-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The source paths of one package.
struct Inputs {
    binary: PathBuf,
    units: PathBuf,
    trust: PathBuf,
    sysusers: PathBuf,
    tmpfiles: PathBuf,
}

impl Inputs {
    fn sources(&self) -> pkg::Sources<'_> {
        pkg::Sources {
            binary: &self.binary,
            units: &self.units,
            trust: &self.trust,
            sysusers: &self.sysusers,
            tmpfiles: &self.tmpfiles,
        }
    }
}

/// `dir/<sub>/<file>` for each file, and the directory.
fn dir_of(dir: &Path, sub: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = dir.join(sub);
    std::fs::create_dir_all(&d).expect("dir");
    for (name, text) in files {
        std::fs::write(d.join(name), text).expect("file");
    }
    d
}

/// A binary, two units, the three trust files, and the two systemd files, in `dir`.
fn inputs(dir: &Path) -> Inputs {
    let binary = dir.join("kbf-installer");
    std::fs::write(&binary, b"the installer").expect("binary");
    let units = dir_of(
        dir,
        "units",
        &[
            ("kbf-installer.service", "[Unit]\n"),
            ("kbf-frontend@.service", "[Unit]\n"),
            ("kbf.slice", "[Slice]\n"),
        ],
    );
    let trust = dir_of(
        dir,
        "trust",
        &[
            ("gh", "gh\n"),
            ("installer.conf", "repo\n"),
            ("trusted_root.jsonl", "{}\n"),
        ],
    );
    let sysusers = dir.join("sysusers.conf");
    std::fs::write(&sysusers, "u kbf\n").expect("sysusers");
    let tmpfiles = dir.join("tmpfiles.conf");
    std::fs::write(&tmpfiles, "d /x\n").expect("tmpfiles");
    Inputs {
        binary,
        units,
        trust,
        sysusers,
        tmpfiles,
    }
}

fn payload_of(i: &Inputs) -> Result<Vec<File>, Error> {
    payload(&pkg::files(&i.sources())?)
}

fn problems_of(files: &[File], tgz: &[u8]) -> Vec<String> {
    check_tarball(tgz, files)
}

#[test]
fn the_payload_is_the_packages_files_at_the_packages_paths_and_modes_sorted() {
    // Catches: a payload at other paths or modes than the .deb's (the pinned gh is
    // 0755 there, a payload giving every trust file 0644 would leave the installer
    // unable to run it), a missing sysusers.d or tmpfiles.d file, or an order that
    // depends on the directory listing.
    let dir = scratch("payload");
    let files = payload_of(&inputs(&dir)).expect("payload");
    let got: Vec<(&str, u32)> = files.iter().map(|f| (f.path.as_str(), f.mode)).collect();
    assert_eq!(
        got,
        [
            ("usr/bin/kbf-installer", 0o755),
            ("usr/lib/systemd/system/kbf-frontend@.service", 0o644),
            ("usr/lib/systemd/system/kbf-installer.service", 0o644),
            ("usr/lib/systemd/system/kbf.slice", 0o644),
            ("usr/lib/sysusers.d/kbf-installer.conf", 0o644),
            ("usr/lib/tmpfiles.d/kbf-installer.conf", 0o644),
            ("usr/share/kbf-installer/trust/gh", 0o755),
            ("usr/share/kbf-installer/trust/installer.conf", 0o644),
            ("usr/share/kbf-installer/trust/trusted_root.jsonl", 0o644),
        ]
    );
}

#[test]
fn the_payload_is_exactly_the_debs_files_with_the_debs_modes_and_bytes() {
    // Catches: the payload and the package drifting apart again: both are read from the
    // real packaging/linux sources here, and the payload's (path, mode, sha256) must be
    // the deb's listing without its directories. A second list of files in the tarball
    // code, or a different mode rule, goes red.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let dir = scratch("same-as-deb");
    let binary = dir.join("kbf-installer");
    std::fs::write(&binary, b"the installer").expect("binary");
    let sources = pkg::Sources {
        binary: &binary,
        units: &root.join("units"),
        trust: &root.join("testdata/trust"),
        sysusers: &root.join("sysusers.d/kbf-installer.conf"),
        tmpfiles: &root.join("tmpfiles.d/kbf-installer.conf"),
    };
    let deb: Vec<String> = pkg::listing(&pkg::files(&sources).expect("deb"))
        .lines()
        .filter(|l| l.starts_with("file "))
        .map(str::to_owned)
        .collect();
    let tar: Vec<String> = payload(&pkg::files(&sources).expect("deb"))
        .expect("payload")
        .iter()
        .map(|f| format!("file {:04o} /{} {}", f.mode, f.path, Sha256::of(&f.bytes)))
        .collect();
    assert_eq!(tar, deb);
    // The tarball itself, read back, holds those paths and modes.
    let files = payload(&pkg::files(&sources).expect("deb")).expect("payload");
    let tgz = tarball(&files).expect("tarball");
    assert_eq!(problems_of(&files, &tgz), Vec::<String>::new());
    let trust_gh = files
        .iter()
        .find(|f| f.path.ends_with("trust/gh"))
        .expect("gh");
    assert_eq!(trust_gh.mode, 0o755);
}

#[test]
fn the_same_inputs_are_the_same_bytes_whatever_their_times_and_order() {
    // Catches: a tarball that records the source files' times or owners, or the order
    // the directories were read in (deterministic output, contract section 9 P11).
    let a = scratch("same-a");
    let b = scratch("same-b");
    let in_a = inputs(&a);
    let in_b = inputs(&b);
    let old = std::fs::File::options()
        .write(true)
        .open(&in_b.binary)
        .expect("open");
    old.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400))
        .expect("mtime");
    let one = payload_of(&in_a).expect("a");
    let two = payload_of(&in_b).expect("b");
    assert_eq!(tarball(&one).expect("a"), tarball(&two).expect("b"));
}

#[test]
fn a_payload_that_is_not_the_contracts_is_refused() {
    // Catches: a glob that matched nothing shipping a payload that starts or trusts
    // nothing, a unit that is not kbf-*.service or kbf.slice, a fourth or a missing
    // trust file, and a sub-directory in a unit directory reaching the host.
    let dir = scratch("refuse");
    let i = inputs(&dir);
    let refused = |what: &str, s: pkg::Sources, why: &str| match pkg::files(&s) {
        Err(Error::Refused(p)) => assert!(
            p.iter().any(|l| l.contains(why)),
            "{what}: {p:?} lacks {why:?}"
        ),
        other => panic!("{what}: {other:?}"),
    };
    let empty = dir_of(&dir, "empty", &[]);
    refused(
        "no units",
        pkg::Sources {
            units: &empty,
            ..i.sources()
        },
        "no unit files",
    );
    let odd = dir_of(
        &dir,
        "odd",
        &[("evil.service", "x"), ("kbf-ok.service", "x")],
    );
    refused(
        "odd unit",
        pkg::Sources {
            units: &odd,
            ..i.sources()
        },
        "evil.service",
    );
    let odd = dir_of(&dir, "odd2", &[("kbf-a.timer", "x")]);
    refused(
        "timer",
        pkg::Sources {
            units: &odd,
            ..i.sources()
        },
        "kbf-a.timer",
    );
    let four = dir_of(
        &dir,
        "four",
        &[
            ("gh", "x"),
            ("installer.conf", "x"),
            ("trusted_root.jsonl", "x"),
            ("extra", "x"),
        ],
    );
    refused(
        "four",
        pkg::Sources {
            trust: &four,
            ..i.sources()
        },
        "extra",
    );
    let two = dir_of(&dir, "two", &[("gh", "x"), ("installer.conf", "x")]);
    refused(
        "two",
        pkg::Sources {
            trust: &two,
            ..i.sources()
        },
        "trusted_root.jsonl is missing",
    );
    let sub = dir_of(&dir, "sub", &[("kbf-a.service", "x")]);
    std::fs::create_dir(sub.join("dir")).expect("sub");
    refused(
        "subdir",
        pkg::Sources {
            units: &sub,
            ..i.sources()
        },
        "not a regular file",
    );
    let missing = dir.join("missing");
    refused(
        "no binary",
        pkg::Sources {
            binary: &missing,
            ..i.sources()
        },
        "missing",
    );
}

fn good_files() -> Vec<File> {
    vec![
        File {
            path: "usr/bin/kbf-installer".to_owned(),
            mode: 0o755,
            bytes: b"bin".to_vec(),
        },
        File {
            path: "usr/lib/systemd/system/a.service".to_owned(),
            mode: 0o644,
            bytes: b"unit".to_vec(),
        },
    ]
}

fn raw_tar(entries: &[(&str, tar::EntryType, u32, u64, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, kind, mode, uid, bytes) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_path(path).expect("path");
        h.set_entry_type(*kind);
        h.set_size(bytes.len() as u64);
        h.set_mode(*mode);
        h.set_uid(*uid);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_cksum();
        tar.append(&h, *bytes).expect("append");
    }
    let tar = tar.into_inner().expect("tar");
    let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(&tar).expect("gzip");
    e.finish().expect("gzip")
}

#[test]
fn what_tarball_writes_check_tarball_accepts() {
    let files = good_files();
    assert_eq!(
        problems_of(&files, &tarball(&files).expect("tarball")),
        Vec::<String>::new()
    );
}

#[test]
fn check_tarball_names_each_way_a_tarball_differs_from_its_files() {
    // Catches: a read-back that cannot fail. One defect per case; each must be named.
    let files = good_files();
    let reg = tar::EntryType::Regular;
    let bin = ("usr/bin/kbf-installer", reg, 0o755, 0, &b"bin"[..]);
    let unit = (
        "usr/lib/systemd/system/a.service",
        reg,
        0o644,
        0,
        &b"unit"[..],
    );
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("not gzip", b"plain".to_vec(), "not a gzip stream"),
        ("missing file", raw_tar(&[bin]), "entries are"),
        (
            "extra file",
            raw_tar(&[bin, unit, ("usr/bin/evil", reg, 0o755, 0, b"x")]),
            "not a file of the payload",
        ),
        ("order", raw_tar(&[unit, bin]), "entries are"),
        (
            "mode",
            raw_tar(&[
                bin,
                ("usr/lib/systemd/system/a.service", reg, 0o666, 0, b"unit"),
            ]),
            "a.service: type, mode",
        ),
        (
            "owner",
            raw_tar(&[
                bin,
                (
                    "usr/lib/systemd/system/a.service",
                    reg,
                    0o644,
                    1000,
                    b"unit",
                ),
            ]),
            "a.service: type, mode",
        ),
        (
            "content",
            raw_tar(&[
                bin,
                ("usr/lib/systemd/system/a.service", reg, 0o644, 0, b"UNIT"),
            ]),
            "the content",
        ),
        (
            "directory",
            raw_tar(&[
                bin,
                (
                    "usr/lib/systemd/system/a.service",
                    tar::EntryType::Directory,
                    0o644,
                    0,
                    b"",
                ),
            ]),
            "a.service: type, mode",
        ),
        (
            "link",
            raw_tar(&[
                bin,
                (
                    "usr/lib/systemd/system/a.service",
                    tar::EntryType::Symlink,
                    0o644,
                    0,
                    b"",
                ),
            ]),
            "a.service: type, mode",
        ),
    ];
    for (label, tgz, want) in cases {
        let problems = problems_of(&files, &tgz);
        assert!(
            problems.iter().any(|p| p.contains(want)),
            "{label}: {problems:?}"
        );
    }
}

fn run_verb(dir: &Path, platform: Platform, commit: &str) -> Result<Vec<String>, Error> {
    let t = inputs(dir);
    installer_tarball(platform, commit, &t.sources(), &dir.join("out"))
}

#[test]
fn the_verb_writes_the_tarball_and_its_sums_line_and_not_twice() {
    // Catches: a tarball not listed in SHA256SUMS.<platform> (the script would find no
    // line), a line with the wrong sum, an asset that overwrites an earlier one, and a
    // second platform clobbering the first's sums file.
    let dir = scratch("verb");
    let printed = run_verb(&dir, Platform::LinuxX86_64, COMMIT).expect("verb");
    let name = format!("kbf-installer-payload-{COMMIT}-linux-x86_64.tar.gz");
    assert_eq!(printed[0], format!("packaged {name}:"));
    let tgz = std::fs::read(dir.join("out").join(&name)).expect("tarball");
    let sums = std::fs::read_to_string(dir.join("out/SHA256SUMS.linux-x86_64")).expect("sums");
    assert_eq!(sums, format!("{}  {name}\n", Sha256::of(&tgz)));
    assert_eq!(printed[1..], [sums.trim_end().to_owned()]);
    let again = run_verb(&dir, Platform::LinuxX86_64, COMMIT);
    assert!(
        matches!(&again, Err(Error::Failed(m)) if m.contains("already lists")),
        "{again:?}"
    );
    run_verb(&dir, Platform::LinuxArm64, COMMIT).expect("arm64");
    assert!(dir.join("out/SHA256SUMS.linux-x86_64").exists());
    assert!(dir.join("out/SHA256SUMS.linux-arm64").exists());
}

#[test]
fn the_verb_refuses_a_mac_and_a_malformed_commit() {
    // Catches: a Mac payload nobody can place, and a name built from a non-commit.
    let dir = scratch("verb-refuse");
    assert!(matches!(
        run_verb(&dir, Platform::DarwinArm64, COMMIT),
        Err(Error::Usage(_))
    ));
    assert_eq!(
        run_verb(&dir, Platform::LinuxArm64, "../x"),
        Err(Error::NotACommit("../x".to_owned()))
    );
    assert!(!dir.join("out").exists(), "refused before writing anything");
}
