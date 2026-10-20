//! `pkg-macos-install-test --package <pkg> [--probe '<command>::<text>'].. `: installs the
//! `kbf-installer` `.pkg` ([`crate::pkg`]) on this Mac, as root (`sudo -n installer`, which
//! a hosted runner allows), and checks what the contract promises of it
//! (docs/design/kbf-installer.md, sections 3 and 7; packaging/macos/README.md). macOS only.
//!
//! 1. Apple's `pkgutil --payload-files` lists exactly the paths this crate's reader
//!    ([`crate::pkg::open`]) finds in the payload: the archive the Rust writers made is
//!    one Apple's tools open and read the same.
//! 2. `installer -pkg <pkg> -target /` succeeds.
//! 3. Every file the package holds is at its path, owned by root:wheel, with its mode
//!    and bytes, and every directory the package adds (the tree under
//!    `/usr/local/kbf-installer`, the state and log directories) is a real directory,
//!    root:wheel, with its mode: the tool's tree is closed to group and other, the state
//!    directory is 0700.
//! 4. Nothing started and nothing was configured: `sudo launchctl print` does not find the
//!    job (and does find a system job that always runs, the control that the finding
//!    means something), no process named `kbf-installer` runs, and the file the job waits
//!    for, [`pkg::GATE`] (`installer.toml` in the layout's configuration directory), does not exist.
//! 5. Each `--probe` command exits 0 and prints its text: the packaged `gh` runs, and the
//!    binary names the commit.
//!
//! There is no uninstall: a Mac package has none, and the runner is thrown away.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use crate::Error;
use crate::package::read;
use crate::pkg::open::{Installed, installed};
use crate::pkg::{self, BINARY};
use crate::pkg_install::Probe;
use crate::pkg_linux::{Out, spawn};

/// A system job that always runs: the control that `launchctl print` finds a loaded job.
const LOADED_CONTROL: &str = "system/com.apple.logd";

fn run(program: &str, args: &[&str]) -> Result<Out, Error> {
    spawn(Command::new(program).args(args))
}

/// The paths `pkgutil --payload-files` prints, without a leading `./` (the root is not one).
fn listed(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|l| l.strip_prefix("./").unwrap_or(l))
        .filter(|l| !l.is_empty() && *l != ".")
        .map(str::to_owned)
        .collect()
}

/// The directories whose owner and mode are the package's to say: the tree it adds under
/// its prefix, and its state and log directories. (`/usr/local` and the like were there
/// before, and are the system's.)
fn ours(path: &str) -> bool {
    path.starts_with(pkg::PREFIX) || path == pkg::STATE_DIR || path == pkg::LOG_DIR
}

/// The `pkg-macos-install-test` verb: the lines to print.
///
/// # Errors
/// A command cannot run, or a check fails (each failure is a line of the refusal).
pub fn run_test(package: &Path, probes: &[Probe]) -> Result<Vec<String>, Error> {
    let bytes = read(package)?;
    let want =
        installed(&bytes).map_err(|e| Error::Failed(format!("{}: {e}", package.display())))?;
    let mut problems = Vec::new();
    let path = package.to_string_lossy();
    let out = run("pkgutil", &["--payload-files", &path])?;
    if !out.ok {
        problems.push(format!("pkgutil --payload-files: {}", out.stderr.trim()));
    } else if listed(&out.stdout) != want.keys().cloned().collect::<BTreeSet<_>>() {
        problems.push(format!(
            "pkgutil lists a different payload than the reader of this crate:\n{}",
            out.stdout
        ));
    }
    let out = run("sudo", &["-n", "installer", "-pkg", &path, "-target", "/"])?;
    if !out.ok || !out.stdout.contains("The install was successful") {
        return Err(Error::Failed(format!(
            "installer: {}\n{}",
            out.stdout.trim(),
            out.stderr.trim()
        )));
    }
    for (rel, node) in &want {
        let at = format!("/{rel}");
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            problems.push(format!("{at} is not installed"));
            continue;
        };
        match node {
            Installed::File { mode, bytes } => {
                if !meta.is_file() {
                    problems.push(format!("{at} is not a regular file"));
                    continue;
                }
                if (meta.uid(), meta.gid(), meta.mode() & 0o7777) != (0, 0, *mode) {
                    problems.push(format!(
                        "{at} is {}:{} {:04o}, not root:wheel {mode:04o}",
                        meta.uid(),
                        meta.gid(),
                        meta.mode() & 0o7777
                    ));
                }
                if std::fs::read(&at).ok().as_deref() != Some(bytes.as_slice()) {
                    problems.push(format!("{at} is not the packaged bytes"));
                }
            }
            Installed::Dir { mode } if ours(rel) => {
                if !meta.is_dir() {
                    problems.push(format!("{at} is not a real directory"));
                } else if (meta.uid(), meta.gid(), meta.mode() & 0o7777) != (0, 0, *mode) {
                    problems.push(format!(
                        "{at} is {}:{} {:04o}, not root:wheel {mode:04o}",
                        meta.uid(),
                        meta.gid(),
                        meta.mode() & 0o7777
                    ));
                }
            }
            Installed::Dir { .. } => {}
        }
    }
    let job = format!("system/{}", pkg::IDENTIFIER);
    if !run("sudo", &["-n", "launchctl", "print", LOADED_CONTROL])?.ok {
        problems.push(format!(
            "control: launchctl print does not find {LOADED_CONTROL}, so \"not loaded\" below means nothing"
        ));
    }
    if run("sudo", &["-n", "launchctl", "print", &job])?.ok {
        problems.push(format!(
            "{job} is loaded: installing the package loads no job"
        ));
    }
    if run(
        "pgrep",
        &[
            "-x",
            pkg::PROGRAM.rsplit('/').next().unwrap_or("kbf-installer"),
        ],
    )?
    .ok
    {
        problems.push("a process named kbf-installer is running".to_owned());
    }
    let gate = pkg::GATE;
    if Path::new(gate).exists() {
        problems.push(format!("{gate} exists: the package configures nothing"));
    }
    for p in probes {
        let argv: Vec<&str> = p.argv.iter().map(String::as_str).collect();
        let out = run(argv[0], &argv[1..])?;
        if !out.ok || !out.stdout.contains(&p.contains) {
            problems.push(format!(
                "`{}` printed {:?} (exit ok: {}), not {:?}",
                p.argv.join(" "),
                out.stdout.trim(),
                out.ok,
                p.contains
            ));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "installed {}: {} paths, {BINARY} and its trust files root:wheel, no job loaded, nothing running",
        package.display(),
        want.len()
    )])
}
