//! `packaging/install.sh`, the curl|sh entry, run against a local HTTP server that
//! serves a release the way the artifacts workflow makes one: payload tarballs written
//! by `cargo xtask installer-tarball` and a `SHA256SUMS` merged by `cargo xtask sums
//! assemble`. Files are placed under a scratch `--root`.
//!
//! The installer in the payload is the STUB (`xtask-fake`, which only has to be an
//! executable with known bytes). The real `kbf-installer` is W3's; the one place that
//! names the stub is [`installer_binary`], so swapping the real binary in is that path.
//! The unit and trust files are stubs too until the packaging/linux/units files land.
//!
//! The script is run by its own path, so its mode and its `#!` line are under test too.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::Digest;
use xtask::package::Platform;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/install.sh");
/// Where the package puts things, and so the payload.
const PAYLOAD: [&str; 5] = [
    "usr/bin/kbf-installer",
    "usr/lib/systemd/system/kbf-frontend@.service",
    "usr/lib/systemd/system/kbf-installer.service",
    "usr/share/kbf-installer/trust/installer.conf",
    "usr/share/kbf-installer/trust/trusted_root.jsonl",
];

/// The installer the payload holds: the stub today.
fn installer_binary() -> &'static str {
    FAKE
}

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("install-sh")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn xtask(args: &[&str]) {
    let out = Command::new(XTASK).args(args).output().expect("run xtask");
    assert!(
        out.status.success(),
        "xtask {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A release: file name to bytes, as `gh release` would hold it.
type Release = HashMap<String, Vec<u8>>;

fn utf8(p: &Path) -> &str {
    p.to_str().expect("utf-8")
}

/// The release of the stub payload for both Linux platforms, built once.
fn release() -> &'static Release {
    static RELEASE: OnceLock<Release> = OnceLock::new();
    RELEASE.get_or_init(|| {
        let dir = scratch("release");
        let units = dir.join("units");
        let trust = dir.join("trust");
        std::fs::create_dir_all(&units).expect("units");
        std::fs::create_dir_all(&trust).expect("trust");
        for (d, name) in [
            (&units, "kbf-installer.service"),
            (&units, "kbf-frontend@.service"),
            (&trust, "installer.conf"),
            (&trust, "trusted_root.jsonl"),
        ] {
            std::fs::write(d.join(name), format!("stub {name}\n")).expect("stub file");
        }
        let out = dir.join("out");
        for p in [Platform::LinuxX86_64, Platform::LinuxArm64] {
            xtask(&[
                "installer-tarball",
                "--binary",
                installer_binary(),
                "--platform",
                p.name(),
                "--commit",
                COMMIT,
                "--units",
                utf8(&units),
                "--trust",
                utf8(&trust),
                "--out",
                utf8(&out),
            ]);
        }
        xtask(&["sums", "assemble", utf8(&out)]);
        std::fs::read_dir(&out)
            .expect("out")
            .map(|e| {
                let e = e.expect("entry");
                (
                    e.file_name().into_string().expect("utf-8"),
                    std::fs::read(e.path()).expect("read"),
                )
            })
            .collect()
    })
}

fn tarball_name(p: Platform) -> String {
    format!("kbf-installer-payload-{COMMIT}-{}.tar.gz", p.name())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The platform of this host, as the script must detect it.
fn host() -> Option<Platform> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(Platform::LinuxX86_64),
        ("linux", "aarch64") => Some(Platform::LinuxArm64),
        _ => None,
    }
}

/// A Linux platform that is not this host's.
fn other_than_host() -> Platform {
    [Platform::LinuxX86_64, Platform::LinuxArm64]
        .into_iter()
        .find(|p| Some(*p) != host())
        .expect("two platforms")
}

/// A server of `files` on 127.0.0.1; the paths it was asked for, in order.
fn serve(files: Release) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            let path = text
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("GET "))
                .and_then(|l| l.split(' ').next())
                .unwrap_or("")
                .to_owned();
            log.lock().expect("log").push(path.clone());
            let body = files.get(path.trim_start_matches('/'));
            let head = match body {
                Some(b) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    b.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned(),
            };
            let _ = stream.write_all(head.as_bytes());
            if let Some(b) = body {
                let _ = stream.write_all(b);
            }
        }
    });
    (url, seen)
}

/// What one run of the script did.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    requests: Vec<String>,
    /// Entries left in the script's temporary directory.
    leftovers: Vec<String>,
    /// The `--root` the script was given.
    root: PathBuf,
}

/// Every path under `dir` with its mode, directories with a trailing `/`.
fn tree(dir: &Path) -> BTreeMap<String, u32> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, u32>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let meta = std::fs::symlink_metadata(e.path()).expect("meta");
            let mut rel = e
                .path()
                .strip_prefix(base)
                .expect("under base")
                .to_string_lossy()
                .into_owned();
            if meta.is_dir() {
                rel.push('/');
            }
            out.insert(rel, meta.permissions().mode() & 0o7777);
            if meta.is_dir() {
                walk(base, &e.path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

impl Run {
    /// The script refused with exit 1 and a message holding `why`, placed nothing and
    /// left no temporary file.
    fn assert_refused(&self, why: &str) {
        assert_eq!(self.code, Some(1), "stderr: {}", self.stderr);
        // The tools the script runs may say their own piece first (curl, tar).
        assert!(
            self.stderr
                .lines()
                .any(|l| l.starts_with("install.sh: ") && l.contains(why)),
            "stderr: {}",
            self.stderr
        );
        assert_eq!(tree(&self.root), BTreeMap::new(), "files were placed");
        assert_eq!(self.leftovers, Vec::<String>::new(), "temporary files left");
    }
}

/// Runs the script against `files` with `--root` an empty directory of its own, after
/// `--release-url` and with `args` next; `extra_path` (a directory) first on PATH when
/// given; `umask` for the script's caller.
fn run_in(name: &str, files: Release, args: &[&str], extra_path: Option<&Path>, root: bool) -> Run {
    let dir = scratch(name);
    let tmp = dir.join("tmp");
    let root_dir = dir.join("root");
    std::fs::create_dir_all(&tmp).expect("tmp");
    std::fs::create_dir_all(&root_dir).expect("root");
    let (url, seen) = serve(files);
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(p) = extra_path {
        let mut joined = p.as_os_str().to_owned();
        joined.push(":");
        joined.push(&path);
        path = joined;
    }
    let mut cmd = Command::new(SCRIPT);
    cmd.arg("--release-url").arg(&url);
    if root {
        cmd.arg("--root").arg(&root_dir);
    }
    let out = cmd
        .args(args)
        .env("TMPDIR", &tmp)
        .env("PATH", path)
        .output()
        .expect("run install.sh");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let leftovers = std::fs::read_dir(&tmp)
        .expect("tmp")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    let requests = seen.lock().expect("log").clone();
    Run {
        code: out.status.code(),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        requests,
        leftovers,
        root: root_dir,
    }
}

fn run(name: &str, files: Release, args: &[&str]) -> Run {
    run_in(name, files, args, None, true)
}

fn good(name: &str, args: &[&str]) -> Run {
    run(name, release().clone(), args)
}

/// The release with the `SHA256SUMS` text changed by `f`.
fn with_sums(f: impl Fn(&str) -> String) -> Release {
    let mut r = release().clone();
    let sums = String::from_utf8(r["SHA256SUMS"].clone()).expect("utf-8");
    r.insert("SHA256SUMS".to_owned(), f(&sums).into_bytes());
    r
}

/// `tgz` as the payload tarball of `p`, summed correctly in the release.
fn with_tarball(p: Platform, tgz: Vec<u8>) -> Release {
    let name = tarball_name(p);
    let old = sha256(&release()[&name]);
    let mut files = with_sums(|s| s.replace(&old, &sha256(&tgz)));
    files.insert(name, tgz);
    files
}

#[test]
fn the_host_payload_is_placed_where_the_package_puts_it_and_nothing_else() {
    // Catches: a script that picks the wrong platform's payload, places a file at
    // another path or mode than the .deb (binary 0755, units and trust files 0644,
    // directories made 0755), places a file the payload does not have, enables or
    // starts a unit, or leaves its temporary directory behind.
    let Some(p) = host() else {
        // A host that is no platform of ours is refused (the detection's other arm).
        good("detect-unsupported", &[]).assert_refused("unsupported");
        return;
    };
    let r = good("detect", &[]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(
        r.requests,
        ["/SHA256SUMS", &format!("/{}", tarball_name(p))]
    );
    let mut want = BTreeMap::new();
    for dir in [
        "usr/",
        "usr/bin/",
        "usr/lib/",
        "usr/lib/systemd/",
        "usr/lib/systemd/system/",
        "usr/share/",
        "usr/share/kbf-installer/",
        "usr/share/kbf-installer/trust/",
    ] {
        want.insert(dir.to_owned(), 0o755);
    }
    for f in PAYLOAD {
        want.insert(f.to_owned(), if f == PAYLOAD[0] { 0o755 } else { 0o644 });
    }
    assert_eq!(tree(&r.root), want);
    assert_eq!(
        std::fs::read(r.root.join(PAYLOAD[0])).expect("binary"),
        std::fs::read(installer_binary()).expect("stub"),
    );
    assert_eq!(
        std::fs::read_to_string(r.root.join(PAYLOAD[2])).expect("unit"),
        "stub kbf-installer.service\n"
    );
    assert!(
        r.stdout.contains("next: sudo kbf-installer setup"),
        "{}",
        r.stdout
    );
    assert_eq!(r.leftovers, Vec::<String>::new(), "temporary files left");
}

#[test]
fn a_restrictive_umask_does_not_narrow_the_directories_it_makes() {
    // Catches: the script not setting its own umask: `usr/share/kbf-installer` made
    // 0700 under the caller's 077 would hide the trust files from the `kbf-verify` user.
    if host().is_none() {
        return;
    }
    let before = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let r = good("umask", &[]);
    rustix::process::umask(before);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    for (path, mode) in tree(&r.root) {
        let want = if path == PAYLOAD[0] || path.ends_with('/') {
            0o755
        } else {
            0o644
        };
        assert_eq!(mode, want, "{path}");
    }
}

#[test]
fn an_upgrade_replaces_the_files_and_leaves_the_directories_alone() {
    // Catches: a payload that carries directory entries (extracting would reset the
    // mode of a host's `usr/bin`), and a placement that fails on an installer already
    // there.
    let Some(_) = host() else { return };
    let dir = scratch("upgrade");
    let root = dir.join("root");
    let bin = root.join("usr/bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::write(bin.join("kbf-installer"), b"old").expect("old");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o750)).expect("chmod");
    let (url, _) = serve(release().clone());
    let out = Command::new(SCRIPT)
        .args(["--release-url", &url, "--root"])
        .arg(&root)
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(bin.join("kbf-installer")).expect("new"),
        std::fs::read(installer_binary()).expect("stub")
    );
    assert_eq!(tree(&root)["usr/bin/"], 0o750);
}

#[test]
fn the_platform_option_selects_that_platforms_payload() {
    // Catches: a script that ignores `--platform`, or reads the tarball name from the
    // wrong SHA256SUMS line (the other platform's line is in the file too).
    let p = other_than_host();
    let r = good("platform", &["--platform", p.name()]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(
        r.requests,
        ["/SHA256SUMS", &format!("/{}", tarball_name(p))]
    );
    assert!(tree(&r.root).contains_key(PAYLOAD[0]));
}

#[test]
fn an_unknown_platform_name_is_refused_before_any_download() {
    // Catches: a platform name passed through into a URL unchecked, and a Mac payload.
    for bad in [
        "plan9-mips",
        "",
        "linux-x86_64/../x",
        "Linux-x86_64",
        "darwin-arm64",
    ] {
        let r = good("bad-platform", &["--platform", bad]);
        r.assert_refused("unsupported platform");
        assert_eq!(r.requests, Vec::<String>::new(), "{bad:?}");
    }
}

#[test]
fn an_unknown_host_is_refused_before_any_download() {
    // Catches: a detection that falls through to some default platform. `uname` is the
    // fake of a riscv64 Linux host.
    let dir = scratch("fake-uname");
    std::fs::copy(FAKE, dir.join("uname")).expect("fake uname");
    let r = run_in("unknown-host", release().clone(), &[], Some(&dir), true);
    r.assert_refused("unsupported platform Linux-riscv64");
    assert_eq!(r.requests, Vec::<String>::new());
}

#[test]
fn without_root_it_refuses_unless_the_files_go_under_a_root_directory() {
    // Catches: a placement into `/` as a user that cannot (a half-placed package), and a
    // `--root` check that wrongly demands root. `id` is the fake of uid 1000.
    let dir = scratch("fake-id");
    std::fs::copy(FAKE, dir.join("id")).expect("fake id");
    let r = run_in("not-root", release().clone(), &[], Some(&dir), false);
    r.assert_refused("must run as root");
    assert_eq!(r.requests, Vec::<String>::new());
    if host().is_some() {
        let r = run_in(
            "not-root-with-root",
            release().clone(),
            &[],
            Some(&dir),
            true,
        );
        assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    }
}

#[test]
fn a_payload_that_does_not_match_its_published_sha_is_refused() {
    // Catches: a script that does not compare, compares the wrong file's sum, or places
    // files before comparing. Two defects: the sum is wrong, and the bytes served are
    // not the ones summed (last byte flipped).
    let p = other_than_host();
    let name = tarball_name(p);
    let right = sha256(&release()[&name]);
    let wrong = with_sums(|s| s.replace(&right, &"0".repeat(64)));
    run("sha-line", wrong, &["--platform", p.name()]).assert_refused("sha256");
    let mut flipped = release().clone();
    *flipped
        .get_mut(&name)
        .and_then(|t| t.last_mut())
        .expect("tarball") ^= 1;
    run("sha-bytes", flipped, &["--platform", p.name()]).assert_refused("sha256");
}

#[test]
fn a_sums_file_without_exactly_one_good_line_for_the_platform_is_refused() {
    // Catches: a script that takes the first of two lines, runs on no line, or accepts
    // a line whose sum is not 64 lowercase hex digits (here 63, and upper case); and a
    // line for another platform or another tarball standing in.
    let p = other_than_host();
    let name = tarball_name(p);
    let sum = sha256(&release()[&name]);
    let line = format!("{sum}  {name}\n");
    let cases: [(&str, Release, &str); 6] = [
        (
            "none",
            with_sums(|s| s.replace(&line, "")),
            "no kbf-installer-payload line",
        ),
        (
            "twice",
            with_sums(|s| format!("{s}{line}")),
            "more than one",
        ),
        (
            "short",
            with_sums(|s| s.replace(&line, &format!("{}  {name}\n", &sum[1..]))),
            "not a sha256",
        ),
        (
            "upper",
            with_sums(|s| s.replace(&line, &format!("{}  {name}\n", sum.to_uppercase()))),
            "not a sha256",
        ),
        (
            "empty",
            with_sums(|_| String::new()),
            "no kbf-installer-payload line",
        ),
        (
            "renamed",
            with_sums(|s| s.replace("kbf-installer-payload-", "kbf-installer-")),
            "no kbf-installer-payload line",
        ),
    ];
    for (label, files, why) in cases {
        run(&format!("sums-{label}"), files, &["--platform", p.name()]).assert_refused(why);
    }
}

#[test]
fn a_download_that_fails_is_refused() {
    // Catches: a script that does not pass curl's failure on (a 404 body saved as the
    // file) for either file.
    let p = other_than_host();
    let mut no_sums = release().clone();
    no_sums.remove("SHA256SUMS");
    run("no-sums", no_sums, &["--platform", p.name()]).assert_refused("cannot download");
    let mut no_tar = release().clone();
    no_tar.remove(&tarball_name(p));
    run("no-tar", no_tar, &["--platform", p.name()]).assert_refused("cannot download");
}

/// A tar entry: its raw path, type and content.
type Entry<'a> = (&'a str, tar::EntryType, &'a [u8]);

/// A gzip tar of `(raw path, type, content)` entries, paths written raw so that `..`
/// and absolute names get in as an attacker would write them.
fn hostile(entries: &[Entry]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, kind, bytes) in entries {
        let mut h = tar::Header::new_ustar();
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_entry_type(*kind);
        h.set_size(if *kind == tar::EntryType::Regular {
            bytes.len() as u64
        } else {
            0
        });
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        if *kind == tar::EntryType::Symlink {
            h.set_link_name("/etc/passwd").expect("link");
        }
        h.set_cksum();
        let body: &[u8] = if *kind == tar::EntryType::Regular {
            bytes
        } else {
            &[]
        };
        tar.append(&h, body).expect("append");
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar.into_inner().expect("tar")).expect("gzip");
    gz.finish().expect("gzip")
}

#[test]
fn a_payload_with_a_file_outside_the_packages_paths_is_refused_and_places_nothing() {
    // Catches: a correctly summed tarball (a compromised release host serves its own
    // SHA256SUMS) that writes outside the root by `..` or an absolute name, plants a
    // file the package does not have, or holds a link or directory entry. The good
    // first entry must not be placed either: refused before extracting.
    let p = other_than_host();
    let reg = tar::EntryType::Regular;
    let good = (PAYLOAD[0], reg, &b"x"[..]);
    let cases: [(&str, Vec<Entry>, &str); 8] = [
        (
            "dotdot",
            vec![good, ("../escape", reg, b"x")],
            "holds the path",
        ),
        (
            "dotdot-mid",
            vec![good, ("usr/bin/../../escape", reg, b"x")],
            "holds the path",
        ),
        (
            "dotdot-end",
            vec![good, ("usr/bin/..", reg, b"x")],
            "holds the path",
        ),
        (
            "absolute",
            vec![good, ("/etc/cron.d/x", reg, b"x")],
            "holds the path",
        ),
        (
            "elsewhere",
            vec![good, ("etc/passwd", reg, b"x")],
            "which the package does not",
        ),
        (
            "other-bin",
            vec![good, ("usr/bin/other", reg, b"x")],
            "which the package does not",
        ),
        (
            "symlink",
            vec![
                good,
                (
                    "usr/lib/systemd/system/a.service",
                    tar::EntryType::Symlink,
                    b"",
                ),
            ],
            "not a regular file",
        ),
        (
            "directory",
            vec![
                good,
                (
                    "usr/lib/systemd/system/sub/",
                    tar::EntryType::Directory,
                    b"",
                ),
            ],
            "not a regular file",
        ),
    ];
    for (label, entries, why) in cases {
        let files = with_tarball(p, hostile(&entries));
        run(
            &format!("hostile-{label}"),
            files,
            &["--platform", p.name()],
        )
        .assert_refused(why);
    }
}

#[test]
fn a_payload_that_is_not_a_tarball_or_is_empty_is_refused() {
    // Catches: a tar failure ignored (an empty listing then passes every check).
    let p = other_than_host();
    run(
        "junk",
        with_tarball(p, b"not a tarball".to_vec()),
        &["--platform", p.name()],
    )
    .assert_refused("not a tar.gz");
    run(
        "empty-tar",
        with_tarball(p, hostile(&[])),
        &["--platform", p.name()],
    )
    .assert_refused("is empty");
}

#[test]
fn the_scripts_own_options_are_checked() {
    // Catches: an option with no value reading past the arguments, a typo silently
    // ignored (a `--rleease-url` would fetch from the default), and a missing root.
    for (args, why) in [
        (&["--release-url"][..], "--release-url needs a value"),
        (&["--platform"][..], "--platform needs a value"),
        (&["--root"][..], "--root needs a value"),
        (
            &["--rleease-url", "x"][..],
            "unknown argument '--rleease-url'",
        ),
        (&["setup"][..], "unknown argument 'setup'"),
        (&["--root", "/does/not/exist"][..], "is not a directory"),
    ] {
        let out = Command::new(SCRIPT)
            .args(args)
            .output()
            .expect("run install.sh");
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.starts_with("install.sh: ") && err.contains(why),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn the_default_release_url_is_this_repositorys_latest_release() {
    // Catches: a default that moved to another repository or to a pinned tag. (The
    // default is not fetched here: no test downloads from the internet.)
    let script = std::fs::read_to_string(SCRIPT).expect("script");
    assert!(
        script.contains(
            "release_url=https://github.com/komira-ai/komira-build-farm/releases/latest/download\n"
        ),
        "the default release URL changed; this test names it on purpose"
    );
}
