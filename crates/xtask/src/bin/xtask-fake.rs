//! `xtask-fake`: what this crate's tests run in place of `cargo` and of a built
//! `kbf-server`, so `build-commit stamps` is tested without building the server.
//!
//! - Run under any name but `kbf-server`, it is cargo: it appends its arguments as one
//!   line to `target/debug/cargo-args` in its working directory, "builds" by
//!   hard-linking itself to `target/debug/kbf-server`, and writes the stamp that build
//!   embeds to `target/debug/kbf-server.stamp`: `KBF_BUILD_COMMIT_OVERRIDE` when set,
//!   else `XTASK_FAKE_HEAD`. An override that is not 12 lowercase hex digits fails the
//!   build with the build script's message.
//! - Run as `kbf-server`, it prints `kbf-server 0.1.0+<stamp>` from the stamp file next
//!   to it.
//!
//! `XTASK_FAKE_DEFECT` plants one defect of the real build: `ignores-override` (the
//! build embeds the head whatever the override), `accepts-bad` (a malformed override
//! builds), `fails-other` (a malformed override fails, but not on the override),
//! `build-fails` (every build fails), `stamp-fails` (every build with an override
//! fails), `vanish` (the build stamped `b2b2b2b2b2b2` deletes the fake cargo, so the
//! next build cannot start) or `version-fails` (`kbf-server` exits 1).

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

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

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().expect("argv0");
    let rest: Vec<String> = args.collect();
    if argv0.ends_with("/kbf-server") {
        server(&argv0)
    } else {
        cargo(&argv0, &rest)
    }
}
