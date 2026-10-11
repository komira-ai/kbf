//! `linux swap`: the host's swapfile for builds, `KBF_SWAP_SIZE` at `KBF_SWAP_FILE`.
//!
//! Builds may swap (no lease gets `memory.swap.max`; the daemon's swap watch kills a
//! lease that swaps at its own cap); MinIO and kbf-server may not (their units set
//! `MemorySwapMax=0`). docs/deploy/linux-build-host.md, "Swap: 128 GB".
//!
//! The plan has each of these only when the host does not have it yet:
//! 1. `fallocate -l <size> <file>`, `chmod 600 <file>`, `mkswap <file>`;
//! 2. `swapon <file>`;
//! 3. a `<file> none swap sw 0 0` line in `/etc/fstab`.
//!
//! An existing file of another size is refused, never resized; one with other
//! permissions gets `chmod 600`. A new file goes only on ext4 or xfs (where `fallocate`
//! makes a file the kernel can swap to) with its size and 16 GiB to spare.
//!
//! It refuses on a host whose kubelet runs (`kubelet`, `k3s`, `k3s-agent`,
//! `rke2-server` or `rke2-agent` active): kubelet's default `failSwapOn` refuses to
//! start with swap on, so the next kubelet restart would take the node out of its
//! cluster. `--kubelet-allows-swap` says the operator has checked that kubelet's
//! config has `failSwapOn: false`.
//!
//! `--remove` is the rollback: `swapoff`, the fstab line dropped (every other line
//! kept), the file removed.

use std::io::Write;

use super::config::LinuxConfig;
use super::host::{
    self, CHMOD, DF, FALLOCATE, FINDMNT, HostError, MKSWAP, SWAPOFF, SWAPON, note, tool,
};
use crate::exec::{Entry, Exec, Kind};
use crate::path::HostPath;
use crate::plan::{Mode, Plan, Planner, RunAs, execute};
use crate::step::{Cmd, Perms};

/// The units whose kubelet refuses to start with swap on.
pub const KUBELETS: [&str; 5] = [
    "kubelet.service",
    "k3s.service",
    "k3s-agent.service",
    "rke2-server.service",
    "rke2-agent.service",
];

const FSTAB: &str = "/etc/fstab";
const GIB: u64 = 1 << 30;
/// What a new swapfile's file system must keep free beside it.
const SPARE_GIB: u64 = 16;

/// What `swap` was asked for.
pub struct Swap {
    pub config: LinuxConfig,
    pub kubelet_allows_swap: bool,
    pub remove: bool,
}

/// `KBF_SWAP_SIZE` in bytes: whole GiB, written `<n>G`, at least 1.
pub fn size_bytes(size: &str) -> Result<u64, HostError> {
    size.strip_suffix('G')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .and_then(|n| n.checked_mul(GIB))
        .ok_or_else(|| HostError::SwapSize(size.to_owned()))
}

/// Plan the swapfile (or its removal), printing what is already so.
pub fn plan<E: Exec>(exec: &E, a: &Swap, out: &mut dyn Write) -> Result<Plan, HostError> {
    let c = &a.config;
    let file = &c.swap_file;
    let want = size_bytes(&c.swap_size)?;
    let active = host::swaps(exec)?.iter().any(|s| s == file.as_str());
    let mut planner = Planner::new(exec);
    let fstab = HostPath::new(FSTAB)?;
    let tab = fstab_now(exec, &fstab)?;
    let listed = tab.2.lines().any(|l| names(l, file));

    if a.remove {
        if active {
            planner.run(on(SWAPOFF, file));
        } else {
            note(out, format_args!("{file}: not active"))?;
        }
        if listed {
            let kept: String = tab
                .2
                .lines()
                .filter(|l| !names(l, file))
                .map(|l| format!("{l}\n"))
                .collect();
            planner.file(fstab, tab.0, tab.1, kept.into_bytes())?;
        } else {
            note(out, format_args!("{FSTAB}: no {file} line"))?;
        }
        if !planner.absent(file.clone())? {
            note(out, format_args!("{file}: absent"))?;
        }
        return Ok(planner.finish());
    }

    for unit in KUBELETS {
        if host::unit_is(exec, "is-active", unit)? {
            if !a.kubelet_allows_swap {
                return Err(HostError::Kubelet(unit));
            }
            note(
                out,
                format_args!(
                    "WARN: {unit} is active; --kubelet-allows-swap says its failSwapOn is false"
                ),
            )?;
        }
    }

    match host::stat(exec, file)? {
        Some(m) if m.kind == Kind::File => {
            if m.len != want {
                return Err(HostError::SwapLen {
                    path: file.clone(),
                    len: m.len,
                    size: c.swap_size.clone(),
                    want,
                });
            }
            note(out, format_args!("{file}: exists, {}", c.swap_size))?;
            if m.perms != Perms::new(0o600) {
                planner.run(chmod_600(file));
            }
        }
        Some(_) => return Err(HostError::NotAFile(file.clone())),
        None => {
            let dir = file
                .as_str()
                .rsplit_once('/')
                .map(|(d, _)| if d.is_empty() { "/" } else { d })
                .unwrap_or("/");
            room(exec, dir, want, &c.swap_size)?;
            planner.run(
                Cmd::new(tool(FALLOCATE))
                    .arg("-l")
                    .arg(&c.swap_size)
                    .arg(file.as_str()),
            );
            planner.run(chmod_600(file));
            planner.run(on(MKSWAP, file));
        }
    }
    if active {
        note(out, format_args!("{file}: active"))?;
    } else {
        planner.run(on(SWAPON, file));
    }
    if listed {
        note(out, format_args!("{FSTAB}: has {file}"))?;
    } else {
        let mut text = tab.2;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("{file} none swap sw 0 0\n"));
        planner.file(fstab, tab.0, tab.1, text.into_bytes())?;
    }
    Ok(planner.finish())
}

/// Plan, then print or carry the plan out.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Swap,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), HostError> {
    let p = plan(&*exec, a, &mut *out)?;
    Ok(execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

/// `/etc/fstab`'s permissions, owner and text; a missing one is root's, 0644, empty.
fn fstab_now<E: Exec>(
    exec: &E,
    fstab: &HostPath,
) -> Result<(Perms, Option<crate::step::Owner>, String), HostError> {
    match host::inspect(exec, fstab)? {
        Some(Entry::File {
            perms,
            owner,
            bytes,
        }) => Ok((
            perms,
            Some(owner),
            String::from_utf8_lossy(&bytes).into_owned(),
        )),
        None => Ok((Perms::new(0o644), None, String::new())),
        Some(_) => Err(HostError::NotAFile(fstab.clone())),
    }
}

/// An fstab line whose first field is `file`.
fn names(line: &str, file: &HostPath) -> bool {
    line.split_whitespace().next() == Some(file.as_str())
}

/// `tool <file>`.
fn on(program: &str, file: &HostPath) -> Cmd {
    Cmd::new(tool(program)).arg(file.as_str())
}

/// `chmod 600 <file>`.
fn chmod_600(file: &HostPath) -> Cmd {
    Cmd::new(tool(CHMOD)).arg("600").arg(file.as_str())
}

/// The file system holding `dir` is ext4 or xfs and has `want` bytes and
/// [`SPARE_GIB`] free.
fn room<E: Exec>(exec: &E, dir: &str, want: u64, size: &str) -> Result<(), HostError> {
    let findmnt = Cmd::new(tool(FINDMNT))
        .arg("-n")
        .arg("-o")
        .arg("FSTYPE")
        .arg("--target")
        .arg(dir);
    let fs = host::checked(exec, &findmnt)?.trim().to_owned();
    if !matches!(fs.as_str(), "ext4" | "xfs") {
        return Err(HostError::FsType {
            dir: dir.to_owned(),
            fs,
        });
    }
    // `df -Pk`: a header, then `<fs> <blocks> <used> <available> <use%> <mount>` in KiB.
    let df = Cmd::new(tool(DF)).arg("-Pk").arg(dir);
    let text = host::checked(exec, &df)?;
    let free_kib = text
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| HostError::Output {
            cmd: df.to_string(),
            out: text.clone(),
        })?;
    if free_kib <= (want + SPARE_GIB * GIB) / 1024 {
        return Err(HostError::NoSpace {
            dir: dir.to_owned(),
            free_gib: free_kib / (1024 * 1024),
            size: size.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "swap_tests.rs"]
mod tests;
