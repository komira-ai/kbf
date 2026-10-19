use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;
use kbf_testkit::dir::TestDir;

const BINARY: &[u8] = b"kbf-daemon's bytes";
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-package-unit"), name)
}

/// `dir/bin/kbf-daemon` holding `BINARY` and `dir/sbom.cdx.json`.
fn inputs(dir: &Path) -> (PathBuf, PathBuf) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let binary = bin.join("kbf-daemon");
    std::fs::write(&binary, BINARY).expect("binary");
    let sbom = dir.join("sbom.cdx.json");
    std::fs::write(&sbom, b"{\"bomFormat\":\"CycloneDX\"}").expect("sbom");
    (binary, sbom)
}

fn gunzip(tgz: &[u8]) -> Vec<u8> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .read_to_end(&mut tar)
        .expect("gunzip");
    tar
}

fn gzip(tar: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(tar).expect("gzip");
    e.finish().expect("gzip")
}

/// The header [`tarball`] writes for `name` and `BINARY`, for a test to plant a
/// defect in.
fn good_header(name: &str) -> tar::Header {
    let mut h = tar::Header::new_ustar();
    h.set_path(name).expect("path");
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(BINARY.len() as u64);
    h.set_mode(MODE);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h
}

/// A gzip tar of `entries`, each a header (its checksum set here) and content.
fn tgz_of(entries: &mut [(tar::Header, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (h, data) in entries.iter_mut() {
        h.set_cksum();
        b.append(h, *data).expect("append");
    }
    gzip(&b.into_inner().expect("tar"))
}

#[test]
fn the_tarball_holds_the_binary_alone_owned_by_root_mode_0755_at_time_0() {
    // Catches: an owner or group other than 0 (an extraction as root would leave the
    // binary owned by the runner's uid), owner names written (mapped by name on the
    // node), a mode other than 0755, a time other than the epoch, a second entry, or
    // bytes other than the binary's. Read from the raw header fields.
    let tgz = tarball("kbf-daemon", BINARY).expect("tarball");
    let tar = gunzip(&tgz);
    let mut archive = tar::Archive::new(&tar[..]);
    let mut entries = archive.entries().expect("entries");
    let mut entry = entries.next().expect("one entry").expect("entry");
    let h = entry.header().clone();
    assert_eq!(&*entry.path_bytes(), b"kbf-daemon");
    assert_eq!(h.entry_type(), tar::EntryType::Regular);
    assert_eq!(h.as_ustar().map(|u| &u.magic[..]), Some(&b"ustar\0"[..]));
    assert_eq!(h.mode().expect("mode"), 0o755);
    assert_eq!((h.uid().expect("uid"), h.gid().expect("gid")), (0, 0));
    assert_eq!(h.mtime().expect("mtime"), 0);
    assert_eq!(
        (h.username_bytes(), h.groupname_bytes()),
        (Some(&b""[..]), Some(&b""[..]))
    );
    let mut held = Vec::new();
    entry.read_to_end(&mut held).expect("content");
    assert_eq!(held, BINARY);
    assert!(entries.next().is_none(), "a second entry");
    assert_eq!(
        check_tarball(&tgz, "kbf-daemon", BINARY),
        Vec::<String>::new()
    );
}

#[test]
fn the_gzip_stream_names_no_file_and_no_time() {
    // Catches: a gzip header carrying the time it was written (so two runs of one
    // commit give different bytes and different SHA256SUMS lines) or a file name.
    // RFC 1952: bytes 4 to 7 are MTIME, bit 3 of byte 3 (FLG) is FNAME.
    let tgz = tarball("kbf-daemon", BINARY).expect("tarball");
    assert_eq!(&tgz[..3], b"\x1f\x8b\x08");
    assert_eq!(tgz[3] & 0x08, 0, "FNAME set");
    assert_eq!(&tgz[4..8], &[0; 4], "MTIME");
}

#[test]
fn one_binary_packages_to_the_same_bytes_whenever_and_wherever_it_is_built() {
    // Catches: the binary file's own time, or the time of packaging, reaching the
    // tarball (the old script took the file's time): the two copies below differ in
    // both, and must give byte-identical tarballs and identical SHA256SUMS lines.
    let mut sums = Vec::new();
    let mut tgzs = Vec::new();
    for (n, age) in [("early", 86_400 * 365), ("late", 5)] {
        let dir = scratch(&format!("same-bytes-{n}"));
        let (binary, sbom) = inputs(&dir);
        let when = SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&binary)
            .and_then(|f| f.set_modified(when))
            .expect("set mtime");
        let out = dir.join("dist");
        package(&binary, &sbom, Platform::LinuxX86_64, COMMIT, &out).expect("package");
        let stem = format!("kbf-daemon-{COMMIT}-linux-x86_64");
        tgzs.push(std::fs::read(out.join(format!("{stem}.tar.gz"))).expect("tgz"));
        sums.push(std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums"));
    }
    assert_eq!(tgzs[0], tgzs[1]);
    assert_eq!(sums[0], sums[1]);
}

#[test]
fn package_writes_both_assets_and_their_sums_lines() {
    // Catches: an asset under another name, the SBOM not copied byte for byte, a sums
    // line naming a path rather than a file name, a line whose digest is not the
    // file's, or a second package into the same directory overwriting the first's
    // lines instead of appending.
    let dir = scratch("writes");
    let (binary, sbom) = inputs(&dir);
    let out = dir.join("dist");
    let printed = package(&binary, &sbom, Platform::LinuxArm64, COMMIT, &out).expect("package");
    let server = dir.join("bin/kbf-server");
    std::fs::write(&server, b"server").expect("server");
    package(&server, &sbom, Platform::LinuxArm64, COMMIT, &out).expect("package");
    let stem = format!("kbf-daemon-{COMMIT}-linux-arm64");
    let tgz = std::fs::read(out.join(format!("{stem}.tar.gz"))).expect("tgz");
    let cdx = std::fs::read(out.join(format!("{stem}.cdx.json"))).expect("cdx");
    assert_eq!(cdx, std::fs::read(&sbom).expect("sbom"));
    let first = format!(
        "{}  {stem}.tar.gz\n{}  {stem}.cdx.json\n",
        Sha256::of(&tgz),
        Sha256::of(&cdx)
    );
    assert_eq!(
        printed,
        [format!("packaged {stem}:")]
            .into_iter()
            .chain(first.lines().map(str::to_owned))
            .collect::<Vec<_>>()
    );
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.linux-arm64")).expect("sums");
    let server_stem = format!("kbf-server-{COMMIT}-linux-arm64");
    assert!(sums.starts_with(&first), "{sums}");
    assert!(
        sums.ends_with(&format!("  {server_stem}.cdx.json\n")),
        "{sums}"
    );
    assert_eq!(sums.lines().count(), 4);
}

#[test]
fn the_system_tar_extracts_exactly_the_binary() {
    // Catches: a tarball the tar a node uses cannot read (GNU tar on Linux, bsdtar
    // on macOS), one that lists more than the binary, or one whose
    // extracted file is not the bytes given or not executable.
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("system-tar");
    let tgz_path = dir.join("a.tar.gz");
    std::fs::write(&tgz_path, tarball("kbf-daemon", BINARY).expect("tarball")).expect("write");
    let list = std::process::Command::new("tar")
        .arg("-tzf")
        .arg(&tgz_path)
        .output()
        .expect("tar -t");
    assert!(list.status.success(), "{list:?}");
    assert_eq!(String::from_utf8_lossy(&list.stdout), "kbf-daemon\n");
    let into = dir.join("x");
    std::fs::create_dir(&into).expect("x");
    let extract = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz_path)
        .arg("-C")
        .arg(&into)
        .status()
        .expect("tar -x");
    assert!(extract.success());
    let file = into.join("kbf-daemon");
    assert_eq!(std::fs::read(&file).expect("extracted"), BINARY);
    let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "{mode:o}");
    // Left here, this executable `kbf-daemon` under target/ is passed to `llvm-cov
    // export` as an object by cargo-llvm-cov, and the coverage report fails on it.
    std::fs::remove_file(&file).expect("remove the extracted copy");
}

#[test]
fn check_tarball_names_each_defect_of_a_planted_tarball() {
    // Catches: the read-back check passing a tarball that is not the one written: each
    // row plants one defect and must be reported by exactly its line.
    let n = "kbf-daemon";
    let p = |what: &str| format!("{n}: {what}");
    let with = |f: fn(&mut tar::Header)| {
        let mut h = good_header(n);
        f(&mut h);
        tgz_of(&mut [(h, BINARY)])
    };
    let mut renamed = good_header("kbf-server");
    renamed.set_cksum();
    let rows: Vec<(&str, Vec<u8>, Vec<String>)> = vec![
        (
            "uid 1000",
            with(|h| h.set_uid(1000)),
            vec![p("the owner is 1000, not 0")],
        ),
        (
            "gid 20",
            with(|h| h.set_gid(20)),
            vec![p("the group is 20, not 0")],
        ),
        (
            "mode 0644",
            with(|h| h.set_mode(0o644)),
            vec![p("the mode is 644, not 755")],
        ),
        (
            "file time",
            with(|h| h.set_mtime(1_760_000_000)),
            vec![p("the time is 1760000000, not 0")],
        ),
        (
            "owner name",
            with(|h| h.set_username("runner").expect("uname")),
            vec![p("the owner name is not empty")],
        ),
        (
            "symlink",
            with(|h| h.set_entry_type(tar::EntryType::Symlink)),
            vec![p("the type is Symlink, not a regular file")],
        ),
        (
            "renamed",
            tgz_of(&mut [(renamed, BINARY)]),
            vec!["kbf-server: the name is not \"kbf-daemon\"".to_owned()],
        ),
        (
            "other bytes",
            tgz_of(&mut [(good_header(n), b"kbf-daemon's BYTES")]),
            vec![p("the content is not the binary's bytes")],
        ),
        (
            "two entries",
            tgz_of(&mut [(good_header(n), BINARY), (good_header(n), BINARY)]),
            vec!["2 entries, not 1".to_owned()],
        ),
        (
            "empty",
            gzip(&[0; 1024]),
            vec!["0 entries, not 1".to_owned()],
        ),
        (
            "not gzip",
            b"not gzip".to_vec(),
            vec!["not a gzip stream: unexpected end of file".to_owned()],
        ),
    ];
    for (what, tgz, want) in rows {
        assert_eq!(check_tarball(&tgz, n, BINARY), want, "{what}");
    }
}

#[test]
fn check_tarball_reports_unreadable_fields_and_a_corrupt_entry() {
    // Catches: a header field that does not parse taken as 0 (an owner field holding
    // garbage would pass as root), or a corrupt second header ignored.
    let mut h = good_header("kbf-daemon");
    {
        let raw = h.as_mut_bytes();
        // ustar: mode at 100, uid 108, gid 116 (8 bytes each), mtime 136 (12).
        for (at, len) in [(100, 8), (108, 8), (116, 8), (136, 12)] {
            raw[at..at + len].fill(b'z');
        }
    }
    let tgz = tgz_of(&mut [(h, BINARY)]);
    let p = |what: &str| format!("kbf-daemon: {what} is unreadable, not");
    let problems = check_tarball(&tgz, "kbf-daemon", BINARY);
    assert_eq!(problems.len(), 4, "{problems:?}");
    for (got, what) in problems
        .iter()
        .zip(["the mode", "the owner", "the group", "the time"])
    {
        assert!(got.starts_with(&p(what)), "{got}");
    }
    // A good entry, then a header block whose checksum is wrong.
    let mut good = good_header("kbf-daemon");
    good.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&good, BINARY).expect("append");
    let mut tar = b.into_inner().expect("tar");
    // In place of the first of the two zero blocks that end the archive.
    let end = tar.len() - 1024;
    tar[end..end + 512].fill(b'x');
    let problems = check_tarball(&gzip(&tar), "kbf-daemon", BINARY);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("not a tar: "), "{problems:?}");
}

#[test]
fn write_tarball_refuses_what_it_reads_back_wrong() {
    // Catches: the read-back after writing skipped, so a tarball that is not the
    // binary is packaged and summed.
    let dir = scratch("read-back");
    let mut h = good_header("kbf-daemon");
    h.set_uid(501);
    let tgz = tgz_of(&mut [(h, BINARY)]);
    let path = dir.join("a.tar.gz");
    assert_eq!(
        write_tarball(&path, &tgz, "kbf-daemon", BINARY),
        Err(Error::Refused(vec![format!(
            "{}: kbf-daemon: the owner is 501, not 0",
            path.display()
        )]))
    );
}

#[test]
fn a_name_that_fits_no_ustar_header_is_refused() {
    // Catches: a long name truncated into the header (the asset would extract under
    // another name).
    let long = "k".repeat(101);
    let e = tarball(&long, BINARY).expect_err("too long");
    assert!(
        e.starts_with(&format!("{long:?} cannot be a tar entry name: ")),
        "{e}"
    );
}

#[test]
fn package_refuses_bad_inputs_and_never_overwrites() {
    // Catches: a non-commit in asset names, a missing binary or empty SBOM packaged,
    // a binary path with no file name, a second package of one binary (its sums lines
    // would repeat), an asset file already there overwritten, and an unreadable or
    // malformed sums file taken as empty.
    let dir = scratch("refuses");
    let (binary, sbom) = inputs(&dir);
    let out = dir.join("dist");
    let pkg = |binary: &Path, sbom: &Path, commit: &str, out: &Path| {
        package(binary, sbom, Platform::DarwinArm64, commit, out)
    };
    assert_eq!(
        pkg(&binary, &sbom, "main", &out),
        Err(Error::NotACommit("main".to_owned()))
    );
    assert_eq!(
        pkg(Path::new("/"), &sbom, COMMIT, &out),
        Err(Error::Failed(
            "/ has no file name to package it as".to_owned()
        ))
    );
    let missing = dir.join("bin/kbf-server");
    assert!(
        pkg(&missing, &sbom, COMMIT, &out).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("read {}: ", missing.display()))),
        "missing binary"
    );
    let empty = dir.join("empty.cdx.json");
    std::fs::write(&empty, b"").expect("empty");
    assert_eq!(
        pkg(&binary, &empty, COMMIT, &out),
        Err(Error::Failed(format!(
            "the SBOM {} is empty",
            empty.display()
        )))
    );
    pkg(&binary, &sbom, COMMIT, &out).expect("first package");
    let sums = out.join("SHA256SUMS.darwin-arm64");
    assert_eq!(
        pkg(&binary, &sbom, COMMIT, &out),
        Err(Error::Failed(format!(
            "{} already lists kbf-daemon-{COMMIT}-darwin-arm64.tar.gz",
            sums.display()
        )))
    );
    // The assets are there, the sums lines are not: no overwrite.
    std::fs::remove_file(&sums).expect("rm sums");
    let tgz = out.join(format!("kbf-daemon-{COMMIT}-darwin-arm64.tar.gz"));
    assert!(
        pkg(&binary, &sbom, COMMIT, &out).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("write {}: ", tgz.display()))),
        "overwrote"
    );
    std::fs::remove_file(&tgz).expect("rm tgz");
    let cdx = out.join(format!("kbf-daemon-{COMMIT}-darwin-arm64.cdx.json"));
    assert!(
        pkg(&binary, &sbom, COMMIT, &out).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("write {}: ", cdx.display()))),
        "overwrote the SBOM"
    );
    std::fs::write(&sums, "not a sums line\n").expect("bad sums");
    assert_eq!(
        pkg(&binary, &sbom, COMMIT, &out),
        Err(Error::Refused(vec![
            "line 1: \"not a sums line\" is not `<sha256>  <file name>` and a newline".to_owned()
        ]))
    );
    std::fs::remove_file(&sums).expect("rm sums");
    std::fs::create_dir(&sums).expect("sums as a directory");
    assert!(
        pkg(&binary, &sbom, COMMIT, &out).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("read {}: ", sums.display()))),
        "a sums path that is a directory"
    );
    // An output directory that cannot be made.
    let blocked = dir.join("file");
    std::fs::write(&blocked, b"").expect("file");
    let under = blocked.join("dist");
    assert!(
        pkg(&binary, &sbom, COMMIT, &under).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("create {}: ", under.display()))),
        "out under a file"
    );
}

#[test]
fn platforms_are_named_as_the_assets_are() {
    // Catches: a platform written under another name than docs/artifacts.md gives
    // (its assets and sums file would not match what the workflow uploads).
    let names: Vec<&str> = Platform::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["darwin-arm64", "linux-x86_64", "linux-arm64"]);
    for p in Platform::ALL {
        assert_eq!(Platform::named(p.name()), Some(p));
    }
    assert_eq!(Platform::named("linux-riscv64"), None);
}
