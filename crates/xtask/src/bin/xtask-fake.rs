//! `xtask-fake`: what this crate's tests run in place of `cargo`, of a built
//! `kbf-server`, of `gh`, of `sudo` and of `podman`, so `build-commit stamps` is tested
//! without building the server, `verify-assets attestations` without GitHub, and
//! `sudo-test` and `podman-tests` without root, systemd or Podman.
//!
//! - Run under any name but `kbf-server`, `gh`, `sudo` or `podman`, it is cargo: it
//!   appends its arguments as one line to `target/debug/cargo-args` in its working
//!   directory, "builds" by hard-linking itself to `target/debug/kbf-server`, and
//!   writes the stamp that build embeds to `target/debug/kbf-server.stamp`:
//!   `KBF_BUILD_COMMIT_OVERRIDE` when set, else `XTASK_FAKE_HEAD`. An override that is
//!   not 12 lowercase hex digits fails the build with the build script's message. With
//!   `--no-run` it builds nothing: it prints the file `cargo-messages` in its working
//!   directory, the JSON messages a test wrote for it.
//! - Run as `kbf-server`, it prints `kbf-server 0.1.0+<stamp>` from the stamp file next
//!   to it.
//! - Run as `gh attestation verify <file> <flags>`, it appends its arguments as one line
//!   to `gh-args` next to it, and verifies the file when `attested` next to it holds
//!   the line `<SHA-256 of the file> <flags>`: the store of attestations, each the file
//!   it covers and the exact flags it verifies under.
//! - Run as `sudo`, it appends `<working directory>: <arguments>` as one line to
//!   `sudo-args` next to it, and exits 0 (the tests it was asked to run passed).
//! - Run as `podman`, it appends `XDG_RUNTIME_DIR=<value>: <arguments>` as one line to
//!   `podman-args` next to it. With no socket at `$XDG_RUNTIME_DIR/bus` (the user
//!   manager's bus, which rootless Podman needs) it exits 125. Otherwise `--version`
//!   prints a version line, `manifest inspect` prints the file `podman-index.json` next
//!   to it (the image index a test wrote for it), and anything else (`pull`) prints
//!   nothing.
//!
//! `XTASK_FAKE_DEFECT` plants one defect of the real build: `ignores-override` (the
//! build embeds the head whatever the override), `accepts-bad` (a malformed override
//! builds), `fails-other` (a malformed override fails, but not on the override),
//! `build-fails` (every build fails), `stamp-fails` (every build with an override
//! fails), `vanish` (the build stamped `b2b2b2b2b2b2` deletes the fake cargo, so the
//! next build cannot start) or `version-fails` (`kbf-server` exits 1). Of the
//! attestation store: `accepts-all` (every file verifies, whatever the flags) or
//! `ignores-bytes` (a file verifies when any attestation has its flags). Of sudo:
//! `tests-fail` (it exits 101, as a failed test binary does) or `fails:<a>,<b>,...`
//! (it exits 101 when one of its arguments is one of `<a>`, `<b>`, ..., so some units
//! of several fail). Of podman: `podman-fails` (every podman command exits 125).
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::ExitCode;

use sha2::Digest;

const OVERRIDE: &str = "KBF_BUILD_COMMIT_OVERRIDE";

fn defect(name: &str) -> bool {
    std::env::var("XTASK_FAKE_DEFECT").is_ok_and(|d| d == name)
}

fn server(argv0: &str) -> ExitCode {
    if defect("version-fails") {
        return ExitCode::FAILURE;
    }
    let stamp = std::fs::read_to_string(format!("{argv0}.stamp")).expect("stamp file");
    println!("kbf-server 0.1.0+{stamp}");
    ExitCode::SUCCESS
}

fn cargo(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new("target/debug");
    std::fs::create_dir_all(dir).expect("target/debug");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("cargo-args"))
        .expect("cargo-args");
    writeln!(log, "{}", args.join(" ")).expect("cargo-args");
    if defect("build-fails") {
        eprintln!("xtask-fake: build failed");
        return ExitCode::from(101);
    }
    // `cargo test --no-run --message-format=json` (sudo-test): the messages the test
    // wrote for it.
    if args.iter().any(|a| a == "--no-run") {
        print!(
            "{}",
            std::fs::read_to_string("cargo-messages").expect("cargo-messages")
        );
        return ExitCode::SUCCESS;
    }
    let head = std::env::var("XTASK_FAKE_HEAD").expect("XTASK_FAKE_HEAD");
    let lower_hex =
        |v: &str| (v.len() == 12) & v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let stamp = match std::env::var(OVERRIDE) {
        Ok(_) if defect("ignores-override") => head,
        Ok(_) if defect("stamp-fails") => {
            eprintln!("error: could not compile");
            return ExitCode::from(101);
        }
        Ok(v) if lower_hex(&v) => v,
        Ok(_) if defect("accepts-bad") => head,
        Ok(_) if defect("fails-other") => {
            eprintln!("error: linking failed");
            return ExitCode::from(101);
        }
        Ok(v) => {
            eprintln!("{OVERRIDE}={v:?}: expected exactly 12 lowercase hex digits");
            return ExitCode::from(101);
        }
        Err(_) => head,
    };
    let bin = dir.join("kbf-server");
    if !bin.exists() {
        std::fs::hard_link(argv0, &bin).expect("link the fake as kbf-server");
    }
    if defect("vanish") && stamp == "b2b2b2b2b2b2" {
        std::fs::remove_file(argv0).expect("remove the fake cargo");
    }
    std::fs::write(dir.join("kbf-server.stamp"), stamp).expect("stamp file");
    eprintln!("xtask-fake: built");
    ExitCode::SUCCESS
}

fn gh(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new(argv0).parent().expect("the fake's directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gh-args"))
        .expect("gh-args");
    writeln!(log, "{}", args.join(" ")).expect("gh-args");
    let file = args
        .get(2)
        .filter(|_| args[..2] == ["attestation", "verify"])
        .expect("gh attestation verify <file> ...");
    let sha256 = hex::encode(sha2::Sha256::digest(std::fs::read(file).expect("the file")));
    let flags = args[3..].join(" ");
    let store = std::fs::read_to_string(dir.join("attested")).unwrap_or_default();
    let by_bytes = !defect("ignores-bytes");
    let attested = store.lines().any(|line| {
        line.split_once(' ')
            .is_some_and(|(sha, f)| ((sha == sha256) | !by_bytes) & (f == flags))
    });
    if attested | defect("accepts-all") {
        return ExitCode::SUCCESS;
    }
    eprintln!("xtask-fake: no attestation of {file} verifies with {flags}");
    ExitCode::FAILURE
}

fn sudo(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new(argv0).parent().expect("the fake's directory");
    let cwd = std::env::current_dir().expect("working directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sudo-args"))
        .expect("sudo-args");
    writeln!(log, "{}: {}", cwd.display(), args.join(" ")).expect("sudo-args");
    let defect_var = std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default();
    let fails_on: Vec<&str> = defect_var
        .strip_prefix("fails:")
        .map(|list| list.split(',').collect())
        .unwrap_or_default();
    if defect("tests-fail") | args.iter().any(|a| fails_on.contains(&a.as_str())) {
        return ExitCode::from(101);
    }
    ExitCode::SUCCESS
}

fn podman(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new(argv0).parent().expect("the fake's directory");
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("podman-args"))
        .expect("podman-args");
    writeln!(log, "XDG_RUNTIME_DIR={runtime_dir}: {}", args.join(" ")).expect("podman-args");
    if defect("podman-fails") {
        eprintln!("xtask-fake: podman failed");
        return ExitCode::from(125);
    }
    let bus = Path::new(&runtime_dir).join("bus");
    if !std::fs::symlink_metadata(&bus).is_ok_and(|m| m.file_type().is_socket()) {
        eprintln!("xtask-fake: no user manager bus at {}", bus.display());
        return ExitCode::from(125);
    }
    match args.first().map(String::as_str) {
        Some("--version") => println!("podman version 0.0.0-xtask-fake"),
        Some("manifest") => print!(
            "{}",
            std::fs::read_to_string(dir.join("podman-index.json")).expect("podman-index.json")
        ),
        _ => {}
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().expect("argv0");
    let rest: Vec<String> = args.collect();
    if argv0.ends_with("/kbf-server") {
        server(&argv0)
    } else if argv0.ends_with("/gh") {
        gh(&argv0, &rest)
    } else if argv0.ends_with("/sudo") {
        sudo(&argv0, &rest)
    } else if argv0.ends_with("/podman") {
        podman(&argv0, &rest)
    } else {
        cargo(&argv0, &rest)
    }
}
