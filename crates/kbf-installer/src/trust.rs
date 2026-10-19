//! The trust list: what the installer believes without checking its content, and the
//! check that the host's bootstrap left it so.
//!
//! The pinned `gh`, the Sigstore trusted root and the host's settings decide whether a
//! release is installed, so whoever can replace one decides. [`trusted_chain`] refuses a
//! path unless it and every directory above it is
//!
//! - there, and not a symbolic link (nor anything but a plain file at the end and plain
//!   directories above);
//! - owned by uid 0;
//! - writable by neither group nor others (mode `& 0o022 == 0`; a sticky world-writable
//!   directory such as `/tmp` is refused too).
//!
//! The questions go to an [`Exec`] (`stat`, which never follows a link), so a test plays
//! the host's owners and modes; on a host it is [`kbf_host::HostExec`]. The same rule is
//! `trusted_chain` of the dev installer this was ported from, with one addition: a
//! directory above the path that is a link is refused, which is why the pins are not
//! under `/etc` (a link on macOS, see [`crate::layout`]).
//!
//! The chain ends at the top-level directory (`/usr`): `/` itself has no [`HostPath`]
//! and is not asked about.
//!
//! A macOS host needs `/usr/local` to be `root:wheel` 755: Homebrew on Intel leaves it
//! writable by the admin group, and the chain is then refused, naming `/usr/local`.

use kbf_host::exec::{Exec, Kind, Meta};
use kbf_host::path::HostPath;

use crate::refusal::{Refusal, TrustFault};

/// `Ok` when `path` (a [`Kind::File`] or a [`Kind::Dir`], as `want` says) and each
/// directory above it is trusted; otherwise the first one that is not, from the path up.
pub fn trusted_chain(exec: &dyn Exec, path: &HostPath, want: Kind) -> Result<(), Refusal> {
    let mut here = Some((path.clone(), want));
    while let Some((p, kind)) = here {
        check(exec, &p, kind)?;
        here = parent(&p).map(|up| (up, Kind::Dir));
    }
    Ok(())
}

/// One path of the chain: [`trusted_chain`] without the parents.
fn check(exec: &dyn Exec, path: &HostPath, want: Kind) -> Result<(), Refusal> {
    let refuse = |fault| Refusal::Untrusted {
        path: path.as_str().into(),
        fault,
    };
    let meta = match exec.stat(path) {
        Ok(Some(meta)) => meta,
        Ok(None) => return Err(refuse(TrustFault::Missing)),
        Err(e) => return Err(refuse(TrustFault::Unreadable(e))),
    };
    verdict(meta, want).map_err(refuse)
}

/// What a path's [`Meta`] says about whether it is trusted.
fn verdict(meta: Meta, want: Kind) -> Result<(), TrustFault> {
    if meta.kind == Kind::Other {
        return Err(TrustFault::Link);
    }
    if meta.kind != want {
        return Err(TrustFault::Kind {
            found: name(meta.kind),
            want: name(want),
        });
    }
    if meta.owner.uid != 0 {
        return Err(TrustFault::NotRoot {
            uid: meta.owner.uid,
        });
    }
    if meta.perms.bits() & 0o022 != 0 {
        return Err(TrustFault::Writable {
            mode: meta.perms.bits(),
        });
    }
    Ok(())
}

/// What a file or a directory is called in a refusal (a link was refused before this).
fn name(kind: Kind) -> &'static str {
    if kind == Kind::Dir {
        "directory"
    } else {
        "file"
    }
}

/// The directory `path` is in; `None` for a top-level path, whose directory is `/`, which
/// a [`HostPath`] cannot name.
fn parent(path: &HostPath) -> Option<HostPath> {
    let up = path.as_str().rsplit_once('/').map(|(up, _)| up);
    up.and_then(|up| HostPath::new(up).ok())
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;
