//! `packaging/install.sh`, the curl|sh entry, run against a local HTTP server that
//! serves a release the way the artifacts workflow makes one: payload tarballs written
//! by `cargo xtask installer-tarball` and a `SHA256SUMS` merged by `cargo xtask sums
//! assemble`. Files are placed under a scratch `--root`.
//!
//! The installer in the payload is the STUB (`xtask-fake`, which only has to be an
//! executable with known bytes). The real `kbf-installer` is W3's; the one place that
//! names the stub is [`installer_binary`], so swapping the real binary in is that path.
//! The unit, trust, sysusers.d and tmpfiles.d files are stubs too (the real ones are
//! checked against the package's list in `installer_tarball/tests.rs`). The host's
//! `systemd-sysusers` and `systemd-tmpfiles` are the fakes of `xtask-fake`, which log
//! their arguments.
//!
//! The script is run by its own path, so its mode and its `#!` line are under test too.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::Digest;
use xtask::package::Platform;

mod support;
use kbf_testkit::dir::TestDir;
use support::output_unbusy;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/install.sh");
/// Where the package puts things, and so the payload.
const PAYLOAD: [&str; 10] = [
    "usr/bin/kbf-installer",
    "usr/lib/systemd/system/kbf-installer.service",
    "usr/lib/systemd/system/kbf-proxy.service",
    "usr/lib/systemd/system/kbf.slice",
    "usr/lib/sysusers.d/kbf-installer.conf",
    "usr/lib/tmpfiles.d/kbf-installer.conf",
    "usr/share/kbf-installer/trust/gh",
    "usr/share/kbf-installer/trust/installer.conf",
    "usr/share/kbf-installer/trust/trusted_root.jsonl",
    "etc/needrestart/conf.d/kbf.conf",
];

/// The mode the deb gives `path`: the installer and the pinned `gh` execute, the rest
/// are 0644.
fn package_mode(path: &str) -> u32 {
    if path == PAYLOAD[0] || path == "usr/share/kbf-installer/trust/gh" {
        0o755
    } else {
        0o644
    }
}

/// The installer the payload holds: the stub today.
fn installer_binary() -> &'static str {
    FAKE
}

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("install-sh"),
        name,
    )
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
            (&units, "kbf-proxy.service"),
            (&units, "kbf.slice"),
            (&trust, "gh"),
            (&trust, "installer.conf"),
            (&trust, "trusted_root.jsonl"),
        ] {
            std::fs::write(d.join(name), format!("stub {name}\n")).expect("stub file");
        }
        let sysusers = dir.join("sysusers.conf");
        let tmpfiles = dir.join("tmpfiles.conf");
        std::fs::write(&sysusers, "stub sysusers\n").expect("sysusers");
        std::fs::write(&tmpfiles, "stub tmpfiles\n").expect("tmpfiles");
        let needrestart = dir.join("needrestart.conf");
        std::fs::write(&needrestart, "stub needrestart\n").expect("needrestart");
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
                "--sysusers",
                utf8(&sysusers),
                "--tmpfiles",
                utf8(&tmpfiles),
                "--needrestart",
                utf8(&needrestart),
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
    /// The lines the fake `systemd-sysusers` and `systemd-tmpfiles` logged.
    systemd: Vec<String>,
    /// The directory `root` is in; last, so it is removed after everything above.
    _dir: TestDir,
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
/// given, then a directory of the fake systemd tools, then the real PATH.
fn run_in(name: &str, files: Release, args: &[&str], extra_path: Option<&Path>, root: bool) -> Run {
    let dir = scratch(name);
    let path = path_with_fake_systemd(&dir);
    run_with_path(dir, files, args, extra_path, root, path)
}

/// Copies the fake to `to` and returns once it can be executed.
///
/// The copy leaves `to` open for writing in any child another test thread forked
/// meanwhile, until that child execs; exec of such a file is `ETXTBSY` (komira-ai/
/// kbf#1073). Callers here do not exec it themselves: `install.sh` does,
/// in a child this test cannot retry. So wait here, once, with a harmless call (the
/// fake run as `uname -s`, by `arg0`). One successful exec means no holder of the write
/// descriptor is left, and none can appear later, since a fork after the copy returned
/// does not inherit a descriptor this process has closed.
fn copy_fake(to: &Path) {
    std::fs::copy(FAKE, to).expect("copy the fake");
    let out =
        output_unbusy(Command::new(to).arg0("uname").arg("-s")).expect("the copied fake runs");
    assert!(out.status.success(), "the copied fake: {:?}", out.status);
}

/// PATH with `dir/fakes` first, holding the fake `systemd-sysusers` and `systemd-tmpfiles`.
fn path_with_fake_systemd(dir: &Path) -> std::ffi::OsString {
    let fakes = dir.join("fakes");
    std::fs::create_dir_all(&fakes).expect("fakes");
    for tool in ["systemd-sysusers", "systemd-tmpfiles"] {
        copy_fake(&fakes.join(tool));
    }
    let mut path = fakes.into_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// [`run_in`] with the whole PATH given (before `extra_path`).
fn run_with_path(
    dir: TestDir,
    files: Release,
    args: &[&str],
    extra_path: Option<&Path>,
    root: bool,
    mut path: std::ffi::OsString,
) -> Run {
    let tmp = dir.join("tmp");
    let root_dir = dir.join("root");
    std::fs::create_dir_all(&tmp).expect("tmp");
    std::fs::create_dir_all(&root_dir).expect("root");
    let (url, seen) = serve(files);
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
        systemd: std::fs::read_to_string(dir.join("fakes/systemd-args"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect(),
        _dir: dir,
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
    // another path or mode than the .deb (binary and pinned gh 0755, units, data
    // files, sysusers.d and tmpfiles.d 0644, directories made 0755), places a file the payload does not have, enables or
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
        "etc/",
        "etc/needrestart/",
        "etc/needrestart/conf.d/",
        "usr/",
        "usr/bin/",
        "usr/lib/",
        "usr/lib/systemd/",
        "usr/lib/systemd/system/",
        "usr/lib/sysusers.d/",
        "usr/lib/tmpfiles.d/",
        "usr/share/",
        "usr/share/kbf-installer/",
        "usr/share/kbf-installer/trust/",
    ] {
        want.insert(dir.to_owned(), 0o755);
    }
    for f in PAYLOAD {
        want.insert(f.to_owned(), package_mode(f));
    }
    assert_eq!(tree(&r.root), want);
    assert_eq!(
        std::fs::read(r.root.join(PAYLOAD[0])).expect("binary"),
        std::fs::read(installer_binary()).expect("stub"),
    );
    assert_eq!(
        std::fs::read_to_string(r.root.join(PAYLOAD[1])).expect("unit"),
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
fn the_users_and_directories_are_made_from_the_placed_files_only_and_after_placing() {
    // Catches: a script that does not apply the two systemd files (a host with units
    // but no `kbf-fetch` user), applies them to anything else, runs them before the
    // files exist (the fakes log what the root held at the time), or loses `--root`.
    let Some(_) = host() else { return };
    let r = good("systemd-calls", &[]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    let root = r.root.display();
    assert_eq!(
        r.systemd,
        [
            format!("systemd-sysusers --root={root} {root}/usr/lib/sysusers.d/kbf-installer.conf"),
            format!(
                "systemd-tmpfiles --root={root} --create {root}/usr/lib/tmpfiles.d/kbf-installer.conf"
            ),
        ]
    );
}

#[test]
fn a_host_without_systemd_tools_is_refused_before_anything_is_placed() {
    // Catches: placing units on a host that cannot make their users (the services would
    // fail at start with "no such user"). PATH is a directory of links to the tools the
    // script needs and none of systemd's.
    let Some(_) = host() else { return };
    let dir = scratch("no-systemd");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let real = std::env::var_os("PATH").unwrap_or_default();
    for tool in [
        "curl",
        "tar",
        "sha256sum",
        "mktemp",
        "uname",
        "id",
        "rm",
        "cat",
    ] {
        let found = std::env::split_paths(&real)
            .map(|d| d.join(tool))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("{tool} is not on PATH"));
        std::os::unix::fs::symlink(found, bin.join(tool)).expect("link");
    }
    let r = run_with_path(
        dir,
        release().clone(),
        &[],
        None,
        true,
        bin.into_os_string(),
    );
    r.assert_refused("systemd-sysusers is not installed");
    assert_eq!(r.requests, Vec::<String>::new());
}

#[test]
fn a_failing_systemd_tool_fails_the_install() {
    // Catches: a script that ignores the exit status of the user and directory step
    // and prints "placed" over a host whose users were not made.
    let Some(_) = host() else { return };
    let dir = scratch("fake-defect");
    let (url, _) = serve(release().clone());
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("root");
    let out = Command::new(SCRIPT)
        .env("XTASK_FAKE_DEFECT", "systemd-fails")
        .env("PATH", path_with_fake_systemd(&dir))
        .args(["--release-url", &url, "--root"])
        .arg(&root)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("install.sh: systemd-sysusers failed"), "{err}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("placed"));
}

#[test]
fn help_prints_the_usage_and_says_envoy_is_a_frontend_prerequisite() {
    let out = Command::new(SCRIPT)
        .arg("--help")
        .output()
        .expect("run install.sh");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "--release-url",
        "--platform",
        "--root",
        "Envoy is not installed here",
        "a frontend host installs Envoy",
    ] {
        assert!(text.contains(want), "missing {want:?}: {text}");
    }
}

#[test]
fn a_restrictive_umask_does_not_narrow_the_directories_it_makes() {
    // Catches: the script not setting its own umask: `usr/share/kbf-installer` made
    // 0700 under the caller's 077 would hide the trust files from every account but root.
    if host().is_none() {
        return;
    }
    let before = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let r = good("umask", &[]);
    rustix::process::umask(before);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    for (path, mode) in tree(&r.root) {
        let want = if path.ends_with('/') {
            0o755
        } else {
            package_mode(&path)
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
        .env("PATH", path_with_fake_systemd(&dir))
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
    copy_fake(&dir.join("uname"));
    let r = run_in("unknown-host", release().clone(), &[], Some(&dir), true);
    r.assert_refused("unsupported platform Linux-riscv64");
    assert_eq!(r.requests, Vec::<String>::new());
}

#[test]
fn a_linux_host_is_mapped_to_the_payload_of_its_machine_name() {
    // Catches: a detection that lacks a name Linux reports. `arm64` is what some Linux
    // kernels and containers print for `uname -m` where most print `aarch64`; dropping
    // the alias refuses those hosts as unsupported. `uname` is the fake, told the
    // machine by `XTASK_FAKE_MACHINE`.
    for (machine, want) in [
        ("x86_64", Platform::LinuxX86_64),
        ("aarch64", Platform::LinuxArm64),
        ("arm64", Platform::LinuxArm64),
    ] {
        let dir = scratch(&format!("machine-{machine}"));
        copy_fake(&dir.join("uname"));
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("root");
        let (url, seen) = serve(release().clone());
        let mut path = dir.path().as_os_str().to_owned();
        path.push(":");
        path.push(path_with_fake_systemd(&dir));
        let out = Command::new(SCRIPT)
            .env("XTASK_FAKE_MACHINE", machine)
            .env("PATH", path)
            .args(["--release-url", &url, "--root"])
            .arg(&root)
            .output()
            .expect("run install.sh");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{machine}: {err}");
        assert_eq!(
            *seen.lock().expect("log"),
            ["/SHA256SUMS", &format!("/{}", tarball_name(want))],
            "{machine}"
        );
    }
}

#[test]
fn without_root_it_refuses_unless_the_files_go_under_a_root_directory() {
    // Catches: a placement into `/` as a user that cannot (a half-placed package), and a
    // `--root` check that wrongly demands root. `id` is the fake of uid 1000.
    let dir = scratch("fake-id");
    copy_fake(&dir.join("id"));
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
    let cases: [(&str, Vec<Entry>, &str); 14] = [
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
                    "usr/lib/systemd/system/kbf-a.service",
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
            "which the package does not",
        ),
        (
            "other-unit",
            vec![good, ("usr/lib/systemd/system/evil.service", reg, b"x")],
            "which the package does not",
        ),
        (
            "timer",
            vec![good, ("usr/lib/systemd/system/kbf-a.timer", reg, b"x")],
            "which the package does not",
        ),
        (
            "nested-unit",
            vec![
                good,
                ("usr/lib/systemd/system/kbf-sub/a.service", reg, b"x"),
            ],
            "which the package does not",
        ),
        (
            "fourth-trust-file",
            vec![good, ("usr/share/kbf-installer/trust/extra", reg, b"x")],
            "which the package does not",
        ),
        (
            "other-sysusers",
            vec![good, ("usr/lib/sysusers.d/evil.conf", reg, b"x")],
            "which the package does not",
        ),
        (
            "other-tmpfiles",
            vec![good, ("usr/lib/tmpfiles.d/evil.conf", reg, b"x")],
            "which the package does not",
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
        script.contains("release_url=https://github.com/komira-ai/kbf/releases/latest/download\n"),
        "the default release URL changed; this test names it on purpose"
    );
}

#[test]
fn the_fake_uname_and_id_answer_only_what_the_script_asks() {
    // Catches: a fake that answers any arguments, so that the script changing to ask
    // something else (`uname -a`, `id -un`) is still served the canned answer and the
    // tests above pass against a question the real tool would answer differently.
    // Run by absolute path as well as by name: `PATH` lookup passes a bare name.
    let dir = scratch("fake-answers");
    for tool in ["uname", "id"] {
        std::fs::copy(FAKE, dir.join(tool)).expect("fake");
    }
    let ask = |tool: &str, arg: &str| {
        let out = output_unbusy(Command::new(dir.join(tool)).arg(arg)).expect("run the fake");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    assert_eq!(ask("uname", "-s"), (true, "Linux\n".to_owned()));
    assert_eq!(ask("uname", "-m"), (true, "riscv64\n".to_owned()));
    assert_eq!(ask("uname", "-a"), (false, String::new()));
    assert_eq!(ask("id", "-u"), (true, "1000\n".to_owned()));
    assert_eq!(ask("id", "-un"), (false, String::new()));
}
