//! What the host-changing verbs (`linux swap`, `mdm`) share: the tools they run, why they
//! refuse, and reads of the host made while planning.
//!
//! Every read goes through [`Exec`] (a query, an `inspect` or a `stat`), in a dry run as
//! in an apply, so a dry run refuses what an apply would.

use std::fmt;
use std::io::{self, Write};
use std::process::Output;

use crate::linux::config::SwapConfigError;
use kbf_host::exec::{Entry, Exec, Meta, succeeded};
use kbf_host::path::{HostPath, PathError};
use kbf_host::plan::Planner;
use kbf_host::step::{Cmd, Owner, Perms, Step};

/// The tools the verbs run, where Debian and Ubuntu put them.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub const SWAPON: &str = "/usr/sbin/swapon";
pub const SWAPOFF: &str = "/usr/sbin/swapoff";
pub const MKSWAP: &str = "/usr/sbin/mkswap";
pub const FALLOCATE: &str = "/usr/bin/fallocate";
pub const FINDMNT: &str = "/usr/bin/findmnt";
pub const DF: &str = "/usr/bin/df";
pub const CHMOD: &str = "/usr/bin/chmod";
pub const SS: &str = "/usr/bin/ss";

/// Why a host-changing verb refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Config(#[from] SwapConfigError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Deploy(#[from] kbf_host::HostError),
    #[error("cannot read {path}: {source}")]
    Inspect { path: HostPath, source: io::Error },
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("{cmd} printed {out:?}, which is not what this kit reads")]
    Output { cmd: String, out: String },
    #[error("cannot write the plan out: {0}")]
    Write(io::Error),

    // swap
    #[error("KBF_SWAP_SIZE={0}: give it in whole GiB, as 128G")]
    SwapSize(String),
    #[error(
        "{0} is active: kubelet refuses to start with swap on unless its config sets failSwapOn: false. Check that first, then re-run with --kubelet-allows-swap"
    )]
    Kubelet(&'static str),
    #[error("{0} is there and is not a regular file")]
    NotAFile(HostPath),
    #[error(
        "{path} exists with {len} bytes, not {size} ({want}): resize it by hand (swapoff, remove, re-run)"
    )]
    SwapLen {
        path: HostPath,
        len: u64,
        size: String,
        want: u64,
    },
    #[error("{dir} is on {fs}: this step makes swapfiles on ext4 or xfs only (fallocate)")]
    FsType { dir: String, fs: String },
    #[error("{dir} has {free_gib} GiB free: {size} and 16 GiB to spare are needed")]
    NoSpace {
        dir: String,
        free_gib: u64,
        size: String,
    },
}

/// A tool's path.
pub fn tool(path: &str) -> HostPath {
    HostPath::new(path).expect("a tool's path is absolute")
}

/// Print one line of what the plan found, before its steps.
pub fn note(out: &mut dyn Write, line: fmt::Arguments<'_>) -> Result<(), HostError> {
    writeln!(out, "{line}").map_err(HostError::Write)
}

/// Run `cmd` as a query; its exit status is the caller's to read.
pub fn query<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Output, HostError> {
    exec.query(cmd).map_err(|source| HostError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

/// Run `cmd` as a query; a non-zero exit is an error naming its stderr. Its output.
pub fn checked<E: Exec>(exec: &E, cmd: &Cmd) -> Result<String, HostError> {
    let out = succeeded(cmd, query(exec, cmd)?).map_err(|source| HostError::Query {
        cmd: cmd.to_string(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd` as a query and tell whether it exited 0.
pub fn exits_0<E: Exec>(exec: &E, cmd: &Cmd) -> Result<bool, HostError> {
    Ok(query(exec, cmd)?.status.success())
}

/// `systemctl <verb> --quiet <unit>` exited 0: `is-active`, `is-enabled`.
pub fn unit_is<E: Exec>(exec: &E, verb: &str, unit: &str) -> Result<bool, HostError> {
    let cmd = Cmd::new(tool(SYSTEMCTL)).arg(verb).arg("--quiet").arg(unit);
    exits_0(exec, &cmd)
}

/// The active swap areas, as `swapon --show` names them.
pub fn swaps<E: Exec>(exec: &E) -> Result<Vec<String>, HostError> {
    let cmd = Cmd::new(tool(SWAPON))
        .arg("--show=NAME")
        .arg("--noheadings");
    Ok(checked(exec, &cmd)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, HostError> {
    exec.inspect(path).map_err(|source| HostError::Inspect {
        path: path.clone(),
        source,
    })
}

pub fn stat<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Meta>, HostError> {
    exec.stat(path).map_err(|source| HostError::Inspect {
        path: path.clone(),
        source,
    })
}

/// `path` joined under the directory `dir`.
pub fn under(dir: &HostPath, name: &str) -> Result<HostPath, HostError> {
    Ok(HostPath::new(&format!("{dir}/{name}"))?)
}

/// Plan `bytes` into `dst` (0755, owned by `root`), keeping a different binary there as
/// `<dst>.previous` (its mode, owned by `root`: a copy left as its old owner's, beside
/// root's binaries, is one rename from being run as root); whether it changes. The
/// `linux` and `mdm` kits install their binaries through it.
pub fn binary<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    dst: &HostPath,
    bytes: Vec<u8>,
    root: Owner,
    out: &mut dyn Write,
) -> Result<bool, HostError> {
    parents(exec, planner, dst)?;
    if let Some(Entry::File {
        perms, bytes: old, ..
    }) = inspect(exec, dst)?
        && old != bytes
    {
        let previous = HostPath::new(&format!("{dst}.previous"))?;
        planner.file(previous, perms, Some(root), old)?;
    }
    let changed = planner.file(dst.clone(), Perms::new(0o755), Some(root), bytes)?;
    if !changed {
        note(out, format_args!("{dst}: unchanged"))?;
    }
    Ok(changed)
}

/// Plan a directory for every missing directory above `path` (root's, 0755) that is
/// not planned yet, so a step that makes `path` finds its parent.
pub fn parents<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    path: &HostPath,
) -> Result<(), HostError> {
    let s = path.as_str();
    for (i, _) in s.match_indices('/').skip(1) {
        let dir = HostPath::new(&s[..i])?;
        let planned = planner
            .steps()
            .iter()
            .any(|step| matches!(step, Step::Mkdir { path, .. } if *path == dir));
        if !planned && stat(exec, &dir)?.is_none() {
            planner.dir(dir, Perms::new(0o755), None)?;
        }
    }
    Ok(())
}
