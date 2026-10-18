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

/// `dir/<sub>/<file>` for each file, and the directory.
fn dir_of(dir: &Path, sub: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = dir.join(sub);
    std::fs::create_dir_all(&d).expect("dir");
    for (name, text) in files {
        std::fs::write(d.join(name), text).expect("file");
    }
    d
}

/// Inputs: a binary, one unit directory and one trust directory.
fn inputs(dir: &Path) -> (PathBuf, Vec<PathBuf>, Vec<PathBuf>) {
    let binary = dir.join("kbf-installer");
    std::fs::write(&binary, b"the installer").expect("binary");
    let units = dir_of(
        dir,
        "units",
        &[
            ("kbf-installer.service", "[Unit]\n"),
            ("kbf-frontend@.service", "[Unit]\n"),
        ],
    );
    let trust = dir_of(
        dir,
        "trust",
        &[("installer.conf", "repo\n"), ("trusted_root.jsonl", "{}\n")],
    );
    (binary, vec![units], vec![trust])
}

fn paths(files: &[File]) -> Vec<&str> {
    files.iter().map(|f| f.path.as_str()).collect()
}

fn problems_of(files: &[File], tgz: &[u8]) -> Vec<String> {
    check_tarball(tgz, files)
}

#[test]
fn the_payload_is_the_packages_files_at_the_packages_paths_sorted() {
    // Catches: a payload at other paths than the .deb's (the two channels would
    // diverge), modes taken from the source files, or an order that depends on the
    // directory listing.
    let dir = scratch("payload");
    let (binary, units, trust) = inputs(&dir);
    let files = payload(&std::fs::read(binary).expect("binary"), &units, &trust).expect("payload");
    assert_eq!(
        paths(&files),
        [
            "usr/bin/kbf-installer",
            "usr/lib/systemd/system/kbf-frontend@.service",
            "usr/lib/systemd/system/kbf-installer.service",
            "usr/share/kbf-installer/trust/installer.conf",
            "usr/share/kbf-installer/trust/trusted_root.jsonl",
        ]
    );
    let modes: Vec<u32> = files.iter().map(|f| f.mode).collect();
    assert_eq!(modes, [0o755, 0o644, 0o644, 0o644, 0o644]);
}

#[test]
fn the_same_inputs_are_the_same_bytes_whatever_their_times_and_order() {
    // Catches: a tarball that records the source files' times or owners, or the order
    // the directories were read in (deterministic output, contract section 9 P11).
    let a = scratch("same-a");
    let b = scratch("same-b");
    let (bin_a, units_a, trust_a) = inputs(&a);
    let (bin_b, units_b, trust_b) = inputs(&b);
    let old = std::fs::File::options()
        .write(true)
        .open(&bin_b)
        .expect("open");
    old.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400))
        .expect("mtime");
    let one = payload(&std::fs::read(bin_a).expect("a"), &units_a, &trust_a).expect("a");
    let two = payload(&std::fs::read(bin_b).expect("b"), &units_b, &trust_b).expect("b");
    assert_eq!(tarball(&one).expect("a"), tarball(&two).expect("b"));
    // Two directories, in either order.
    let more = dir_of(&a, "more", &[("kbf-envoy@.service", "[Unit]\n")]);
    let ab = payload(b"x", &[units_a[0].clone(), more.clone()], &trust_a).expect("ab");
    let ba = payload(b"x", &[more, units_a[0].clone()], &trust_a).expect("ba");
    assert_eq!(ab, ba);
}

#[test]
fn a_payload_with_no_unit_or_no_trust_file_or_a_repeated_path_is_refused() {
    // Catches: a glob that matched nothing shipping a payload that starts or trusts
    // nothing, and a file of one directory silently replacing another's.
    let dir = scratch("refuse");
    let (_, units, trust) = inputs(&dir);
    let empty = dir_of(&dir, "empty", &[]);
    assert_eq!(
        payload(b"x", &[], &trust),
        Err(Error::Failed("no unit file in --units".to_owned()))
    );
    assert_eq!(
        payload(b"x", std::slice::from_ref(&empty), &trust),
        Err(Error::Failed("no unit file in --units".to_owned()))
    );
    assert_eq!(
        payload(b"x", &units, &[empty]),
        Err(Error::Failed("no trust file in --trust".to_owned()))
    );
    let again = dir_of(&dir, "again", &[("kbf-installer.service", "other\n")]);
    assert_eq!(
        payload(b"x", &[units[0].clone(), again], &trust),
        Err(Error::Failed(
            "usr/lib/systemd/system/kbf-installer.service is in two directories".to_owned()
        ))
    );
}

#[test]
fn only_regular_files_with_plain_names_are_taken() {
    // Catches: a subdirectory or a name with a space or a leading dot reaching the
    // host's unit directory.
    let dir = scratch("names");
    let (_, _, trust) = inputs(&dir);
    for (label, name) in [
        ("hidden", ".hidden"),
        ("space", "a b.service"),
        ("slash-ish", "a$b"),
    ] {
        let units = dir_of(&dir, &format!("units-{label}"), &[(name, "x")]);
        let got = payload(b"x", std::slice::from_ref(&units), &trust);
        assert!(
            matches!(&got, Err(Error::Failed(m)) if m.contains("not a file name")),
            "{name:?}: {got:?}"
        );
    }
    let units = dir_of(&dir, "units-sub", &[("ok.service", "x")]);
    std::fs::create_dir(units.join("sub")).expect("sub");
    let got = payload(b"x", &[units], &trust);
    assert!(
        matches!(&got, Err(Error::Failed(m)) if m.contains("not a regular file")),
        "{got:?}"
    );
    let got = payload(b"x", &[dir.join("missing")], &trust);
    assert!(matches!(got, Err(Error::Spawn { .. })), "{got:?}");
}

fn good_files() -> Vec<File> {
    vec![
        File {
            path: BINARY_PATH.to_owned(),
            mode: 0o755,
            bytes: b"bin".to_vec(),
        },
        File {
            path: format!("{UNIT_DIR}/a.service"),
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
    let bin = (BINARY_PATH, reg, 0o755, 0, &b"bin"[..]);
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
    let (binary, units, trust) = inputs(dir);
    installer_tarball(&binary, platform, commit, &units, &trust, &dir.join("out"))
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
