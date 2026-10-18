//! `cargo xtask image-check --image <ref> [--summary <file>]`: the frontend container
//! image (`deploy/k8s/image/Dockerfile`) does what the Deployment relies on.
//!
//! Three checks, each one `docker` call on a local image:
//! - `docker image inspect` says the image's user is [`USER`], the non-root uid the
//!   manifests run as;
//! - `docker run <image> --version` exits 0 and prints a `kbf <version>+<commit>` line,
//!   so the binary starts on the base's libraries;
//! - `docker run <image> --mode=frontend --check`, with no `/etc/kbf/kbf.toml` in the
//!   image, exits [`REFUSED`] (2) and says on stderr that it cannot read
//!   [`CONFIG_PATH`]. The exit status alone is not enough: clap's usage error for a flag
//!   the binary does not know (an image built from a tree without `--mode`) also exits
//!   2, and a missing library exits 127 or 1.
//!
//! The image id is appended to `--summary`. Nothing is pushed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// The user the image runs as (`deploy/k8s/image/Dockerfile`), and the manifests' uid.
pub const USER: &str = "65532:65532";
/// The exit status of a command line or config `kbf` refuses.
pub const REFUSED: i32 = 2;
/// The file the frontend refuses to start without, as `kbf` names it in its refusal.
pub const CONFIG_PATH: &str = "/etc/kbf/kbf.toml";

/// What one `docker` call did.
#[derive(Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit status; `None` when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// `docker image inspect --format '{{.Id}} {{.Config.User}}'`'s answer: the image id.
///
/// # Errors
/// The call failed, or the user is not [`USER`].
pub fn judge_inspect(ran: &Ran) -> Result<String, Error> {
    if ran.code != Some(0) {
        return Err(Error::Failed(format!(
            "docker image inspect exited {:?}",
            ran.code
        )));
    }
    let (id, user) = ran
        .stdout
        .trim()
        .split_once(' ')
        .unwrap_or((ran.stdout.trim(), ""));
    if user != USER {
        return Err(Error::Failed(format!(
            "the image runs as {user:?}, not {USER}"
        )));
    }
    Ok(id.to_owned())
}

/// `kbf --version`'s answer.
///
/// # Errors
/// The exit status is not 0, or the first line is not `kbf <version>+<commit>`.
pub fn judge_version(ran: &Ran) -> Result<(), Error> {
    if ran.code != Some(0) {
        return Err(Error::Failed(format!(
            "`kbf --version` in the image exited {:?}",
            ran.code
        )));
    }
    let line = ran.stdout.lines().next().unwrap_or("");
    let stamp = line.strip_prefix("kbf ").and_then(|v| v.split_once('+'));
    match stamp {
        Some((version, commit)) if !version.is_empty() && !commit.is_empty() => Ok(()),
        _ => Err(Error::Failed(format!(
            "`kbf --version` in the image printed {line:?}, not `kbf <version>+<commit>`"
        ))),
    }
}

/// `kbf --mode=frontend --check`'s answer with no config in the image: exit
/// [`REFUSED`] and a stderr that says it cannot read [`CONFIG_PATH`].
///
/// # Errors
/// The exit status is not [`REFUSED`], or stderr does not name the missing config (the
/// binary refused for another reason, such as an unknown `--mode` flag).
pub fn judge_refusal(ran: &Ran) -> Result<(), Error> {
    if ran.code != Some(REFUSED) {
        return Err(Error::Failed(format!(
            "`kbf --mode=frontend --check` with no kbf.toml exited {:?}, not {REFUSED}",
            ran.code
        )));
    }
    let first = ran.stderr.lines().next().unwrap_or("");
    if ran.stderr.contains("cannot read") && ran.stderr.contains(CONFIG_PATH) {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "`kbf --mode=frontend --check` with no kbf.toml exited {REFUSED} but did not \
             say it cannot read {CONFIG_PATH} (stderr starts {first:?}): it refused for \
             another reason"
        )))
    }
}

fn docker(program: &Path, args: &[&str]) -> Result<Ran, Error> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(Error::spawn(format!("run {}", program.display())))?;
    Ok(Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The three checks on `image`; the lines to print, and the id appended to `summary`.
///
/// # Errors
/// A check failed, or `docker` could not be run, or the summary not written.
pub fn run(
    docker_program: &Path,
    image: &str,
    summary: Option<&PathBuf>,
) -> Result<Vec<String>, Error> {
    let id = judge_inspect(&docker(
        docker_program,
        &[
            "image",
            "inspect",
            "--format",
            "{{.Id}} {{.Config.User}}",
            image,
        ],
    )?)?;
    judge_version(&docker(
        docker_program,
        &["run", "--rm", image, "--version"],
    )?)?;
    judge_refusal(&docker(
        docker_program,
        &["run", "--rm", image, "--mode=frontend", "--check"],
    )?)?;
    if let Some(file) = summary {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(file)
            .map_err(Error::spawn(format!("open {}", file.display())))?;
        writeln!(f, "kbf-frontend image id: `{id}`")
            .map_err(Error::spawn(format!("write {}", file.display())))?;
    }
    Ok(vec![format!(
        "{image}: {id}: runs as {USER}, starts, refuses to run without a config"
    )])
}
