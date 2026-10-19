//! P7 of `docs/design/kbf-installer.md` (section 5, "Fetch child"): pulling one component's
//! tarball onto the host without trusting anything on the way.
//!
//! The root parent re-executes its own binary as `kbf-installer fetch-child` under the `kbf-fetch`
//! user, confined ([`crate::run::Confine`]); the child connects to the one configured source
//! and writes the tarball to standard output; the parent counts and hashes the bytes as they
//! arrive and writes them to a file it created itself ([`pipe`]). The child has no object it
//! can write, swap or link, and its exit status is no input: the bytes are judged by their
//! count and their sha256.
//!
//! - [`Source`]: the one place bytes may come from, read from `installer.toml` and never from
//!   an order: `url` (an https file server, `files/<sha256>`) or `frontend`
//!   (`ReleaseService.Fetch` over mutual TLS with the node's certificate, lent over a
//!   descriptor, [`identity`]).
//! - [`child`]: the `fetch-child` verb, the only code of this crate that touches a network. It
//!   follows no redirect, and stops at the byte limit it is given.
//! - [`pipe`]: the parent's end: the child started confined, read with `poll`, ended at a cap.
//! - [`fetch_component`]: the parent's whole step: the checks that need no byte (digest and
//!   size well formed, size within the host's cap, room on the disk), the child, and the
//!   comparison of what arrived with what the order named.
//! - [`describe_release`]: what a `frontend_update` order, which names a commit and nothing
//!   else, needs first: the same child, asked what the commit holds ([`describe`]). The
//!   source's bytes are decoded in the child and the parent reads a small fixed form.
//!
//! What this module does **not** do is the rest of section 5's order (attestation of the file,
//! its shape, the binary's own report, the install): those are the verify pipeline's
//! (`stage`, P8), which takes the file written here; `helper::fetcher_real` is the joint
//! between the two. Nothing written here is root's authority until that pipeline has accepted it.

pub mod child;
pub mod describe;
pub mod identity;
pub mod pipe;
pub mod user;

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::CommitId;
use crate::layout::Platform;
use crate::run::{Confine, RunAs};
use describe::{Answer, DescribeFault, Described};
use identity::{Identity, IdentityFault};
use kbf_types::ReleasePlatform;
use pipe::{Caps, ChildSpec, PipeFault, Received};
use user::UserFault;

/// The file name of the unprivileged account the child runs as (Linux; `_kbf-fetch` on macOS).
pub const FETCH_USER: &str = "kbf-fetch";
/// The longest the child waits to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a fetch goes without a byte.
pub const NO_BYTE_TIMEOUT: Duration = Duration::from_secs(60);
/// The most a component may be, whatever the settings say (the store's own cap).
pub const MAX_COMPONENT_CEILING: u64 = 1 << 30;

/// Where the bytes come from. Chosen by `installer.toml`; an order, an offer, the environment
/// and the frontend cannot change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// An https file server serving the release store's layout, `<base>/files/<sha256>`. The
    /// system's roots authenticate the transport only; the bytes are trusted by verification.
    Url {
        /// `https://host[:port][/path]`, as `installer.toml` validated it.
        base: String,
        /// Roots to trust instead of the system's (a mirror under a private CA). Not a setting
        /// of `installer.toml` today; the tests use it.
        ca_file: Option<PathBuf>,
    },
    /// The frontend's `ReleaseService.Fetch` on its worker listener, over mutual TLS.
    Frontend(FrontendSource),
}

/// The frontend as a source: its address and what authenticates both ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendSource {
    /// `https://host[:port]` of the worker listener; the host is the name checked against the
    /// certificate's SAN.
    pub url: String,
    /// The pinned CA that enrollment wrote.
    pub ca_file: PathBuf,
    /// The node's certificate and key (`installer.toml`'s `node_cert`, `node_key`).
    pub node_cert: PathBuf,
    pub node_key: PathBuf,
}

/// The host's bounds on a fetch (`installer.toml`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `max_component_bytes`: at most [`MAX_COMPONENT_CEILING`].
    pub max_component_bytes: u64,
    /// `fetch_timeout_s` as a duration: the whole of one component.
    pub fetch_timeout: Duration,
}

/// One component to pull. A locator and claims: the order named them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component<'a> {
    /// The tarball's sha256 (64 lowercase hex), also its name at the source.
    pub sha256: &'a str,
    /// The tarball's length in bytes.
    pub size: u64,
}

/// The program that fetches, and who it runs as.
#[derive(Clone, Debug)]
pub struct ChildProgram {
    /// The installer's own binary (an absolute path).
    pub program: PathBuf,
    /// The fetch user, whom the child is switched to and whose other processes are killed at
    /// each start, so it must be an account that runs nothing else (`kbf-fetch`: a system user
    /// that owns nothing). `None` is for a test that cannot switch user (the child is then
    /// whoever the caller is); a host always names it. A child that would be root, named or
    /// not, is refused ([`pipe::PipeFault::Root`]).
    pub run_as: Option<RunAs>,
}

/// Why the fetch child's program could not be had.
#[derive(Debug, thiserror::Error)]
pub enum ProgramFault {
    #[error("cannot tell which file this program runs from: {0}")]
    Exe(io::Error),
    #[error("the fetch user: {0}")]
    User(#[from] UserFault),
}

impl ChildProgram {
    /// This program's own file, to be run as the fetch user of `platform`. A host without
    /// that user (or with it as root) has no fetch child.
    pub fn of_this_program(platform: Platform) -> Result<ChildProgram, ProgramFault> {
        ChildProgram::of_this_program_as(user::name(platform))
    }

    fn of_this_program_as(user: &str) -> Result<ChildProgram, ProgramFault> {
        Ok(ChildProgram {
            program: std::env::current_exe().map_err(ProgramFault::Exe)?,
            run_as: Some(user::lookup(user)?),
        })
    }
}

/// Why a fetch did not end in a file of exactly the named bytes. Nothing but the file the
/// caller gave has changed, and that file is the caller's to remove.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("the component's sha256 {0:?} is not 64 lowercase hex digits")]
    BadDigest(String),
    #[error("the component is declared {size} bytes; at least 1 and at most {cap}")]
    Size { size: u64, cap: u64 },
    #[error("{need} bytes are needed under {} and {free} are free", path.display())]
    Disk { need: u64, free: u64, path: PathBuf },
    #[error("the release source {0:?} is refused: {1}")]
    Source(String, &'static str),
    #[error("{0}")]
    Identity(#[from] IdentityFault),
    #[error("{0}")]
    Pipe(#[from] PipeFault),
    #[error("the download has sha256 {actual}; the order names {want}")]
    Digest { actual: String, want: String },
    #[error("{0}")]
    Describe(#[from] DescribeFault),
}

impl FetchError {
    /// The contract's stable refusal word (section 5, "Refusals").
    pub fn word(&self) -> &'static str {
        match self {
            FetchError::BadDigest(_) | FetchError::Digest { .. } => "hash",
            FetchError::Size { .. } => "size",
            FetchError::Disk { .. } => "disk",
            FetchError::Source(..)
            | FetchError::Identity(_)
            | FetchError::Pipe(_)
            | FetchError::Describe(_) => "fetch",
        }
    }
}

pub(crate) fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The bytes the disk must have free before anything is opened: the tarball, its extraction
/// (1 GiB, or ten times the declared size if that is smaller) and the release it replaces.
pub fn room_needed(size: u64, replaced: u64) -> u64 {
    size.saturating_add(MAX_COMPONENT_CEILING.min(size.saturating_mul(10)))
        .saturating_add(replaced)
}

/// Fail with [`FetchError::Disk`] unless `need` bytes are free on the file system holding `dir`.
pub fn ensure_room(dir: &Path, need: u64) -> Result<(), FetchError> {
    let stat = rustix::fs::statvfs(dir)
        .map_err(|e| FetchError::Pipe(PipeFault::Write(io::Error::from(e))))?;
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    if free < need {
        return Err(FetchError::Disk {
            need,
            free,
            path: dir.to_path_buf(),
        });
    }
    Ok(())
}

/// The flags of `fetch-child` that name where the bytes come from: the kind and address of
/// `source`, the roots a `url` source is checked against, the identity to lend (frontend only).
fn source_flags(source: &Source, lend: Option<&OwnedFd>) -> Vec<OsString> {
    use std::os::fd::AsRawFd;
    match source {
        Source::Url { base, ca_file } => {
            let mut flags: Vec<OsString> =
                vec!["--kind".into(), "url".into(), "--url".into(), base.into()];
            if let Some(ca) = ca_file {
                flags.extend(["--ca-file".into(), ca.into()]);
            }
            flags
        }
        Source::Frontend(f) => {
            let fd = lend
                .expect("a frontend fetch lends the identity")
                .as_raw_fd();
            vec![
                "--kind".into(),
                "frontend".into(),
                "--url".into(),
                f.url.clone().into(),
                "--identity-fd".into(),
                fd.to_string().into(),
            ]
        }
    }
}

/// The flags of `fetch-child` for a component of `source`.
fn child_flags(source: &Source, c: &Component<'_>, lend: Option<&OwnedFd>) -> Vec<OsString> {
    // One byte more than the declared size, so that a source that sends more is seen to.
    let limit = c.size + 1;
    let mut flags: Vec<OsString> = vec![
        "--sha256".into(),
        c.sha256.into(),
        "--limit".into(),
        limit.to_string().into(),
    ];
    flags.extend(source_flags(source, lend));
    flags
}

/// The flags of `fetch-child` that ask `source` what `commit` holds. A frontend is also told
/// the platform (it describes only that one); a `url` source's file lists them all.
fn describe_flags(
    source: &Source,
    commit: &CommitId,
    platform: ReleasePlatform,
    lend: Option<&OwnedFd>,
) -> Vec<OsString> {
    let mut flags: Vec<OsString> = vec![
        "--describe".into(),
        commit.as_str().into(),
        "--limit".into(),
        // One byte more than the most, so that a child that writes more is seen to.
        (describe::MAX_BYTES + 1).to_string().into(),
    ];
    if matches!(source, Source::Frontend(_)) {
        flags.extend(["--platform".into(), platform.as_str().into()]);
    }
    flags.extend(source_flags(source, lend));
    flags
}

/// The identity a `frontend` source presents, read now (so a renewed certificate is used)
/// and lent on a descriptor; none for a `url` source.
fn lend_identity(source: &Source) -> Result<Option<OwnedFd>, FetchError> {
    match source {
        Source::Frontend(f) => Ok(Some(
            Identity::read(&f.node_cert, &f.node_key, &f.ca_file)?.lend()?,
        )),
        Source::Url { .. } => Ok(None),
    }
}

/// The child as `Confine::FETCH` confines it: the installer's own program, `fetch-child`.
fn child_spec(child: &ChildProgram) -> ChildSpec {
    ChildSpec {
        program: child.program.clone(),
        args: vec!["fetch-child".into()],
        run_as: child.run_as,
        confine: Confine::FETCH,
    }
}

/// Validate `source` as the settings would have: https, no user information, query or fragment;
/// the frontend's with no path either.
pub fn check_source(source: &Source) -> Result<(), FetchError> {
    match source {
        Source::Url { base, .. } => {
            let clean = crate::installer_toml::https_base(base)
                .map_err(|_| FetchError::Source(base.clone(), "not a plain https base URL"))?;
            if clean != *base {
                return Err(FetchError::Source(
                    base.clone(),
                    "not in the form settings write",
                ));
            }
        }
        Source::Frontend(f) => {
            let clean = crate::installer_toml::https_base(&f.url)
                .map_err(|_| FetchError::Source(f.url.clone(), "not a plain https URL"))?;
            if clean != f.url || f.url[8..].contains('/') {
                return Err(FetchError::Source(
                    f.url.clone(),
                    "must be https://host[:port]",
                ));
            }
        }
    }
    Ok(())
}

/// Pull `component` from `source` into `out` (a new file the caller made in a root-owned
/// scratch directory), through the confined child, and return what arrived once it is exactly
/// the declared number of bytes of the named sha256.
///
/// `room` is the directory whose file system must have [`room_needed`] bytes free and the
/// bytes of the release being replaced; it is checked before the file or the child exists.
pub fn fetch_component(
    child: &ChildProgram,
    source: &Source,
    limits: Limits,
    component: &Component<'_>,
    room: (&Path, u64),
    out: &mut File,
) -> Result<Received, FetchError> {
    if !is_digest(component.sha256) {
        return Err(FetchError::BadDigest(component.sha256.to_owned()));
    }
    let cap = limits.max_component_bytes.min(MAX_COMPONENT_CEILING);
    if component.size == 0 || component.size > cap {
        return Err(FetchError::Size {
            size: component.size,
            cap,
        });
    }
    check_source(source)?;
    // Before the room is measured and the identity read: no key is read for a child that
    // would be root. (`receive` asks again; this is the earlier place.)
    pipe::refuse_root(child.run_as, pipe::own_ids())?;
    ensure_room(room.0, room_needed(component.size, room.1))?;
    let lent = lend_identity(source)?;
    let flags = child_flags(source, component, lent.as_ref());
    let caps = Caps {
        size: component.size,
        no_byte: NO_BYTE_TIMEOUT.min(limits.fetch_timeout),
        total: limits.fetch_timeout,
    };
    let got = pipe::receive(
        &child_spec(child),
        &flags,
        lent.as_ref().map(AsFd::as_fd),
        caps,
        out,
    )?;
    // The lent descriptor is closed as soon as the child is done with it.
    drop(lent);
    if got.sha256 != component.sha256 {
        return Err(FetchError::Digest {
            actual: got.sha256,
            want: component.sha256.to_owned(),
        });
    }
    Ok(got)
}

/// Ask `source` what `commit` holds, through the confined child, and give back the components
/// a host of `platform` is to fetch (a tarball and the bundle that attests it, each by digest).
///
/// The child decodes the source's answer and the parent reads the fixed form it writes
/// ([`describe`]); the parent holds that to the order's commit, the host's platform and the
/// bounds of a release before it names a file. `limits.max_component_bytes` is the most a
/// tarball may be, as for [`fetch_component`]. A source that says it will not describe the
/// commit, or an answer that cannot be used, is [`FetchError::Describe`]. A source that cannot
/// be reached is a pipe fault.
pub fn describe_release(
    child: &ChildProgram,
    source: &Source,
    limits: Limits,
    commit: &CommitId,
    platform: ReleasePlatform,
) -> Result<Vec<Described>, FetchError> {
    check_source(source)?;
    // Before the identity is read: no key is read for a child that would be root.
    pipe::refuse_root(child.run_as, pipe::own_ids())?;
    let lent = lend_identity(source)?;
    let flags = describe_flags(source, commit, platform, lent.as_ref());
    let caps = Caps {
        size: describe::MAX_BYTES,
        no_byte: NO_BYTE_TIMEOUT.min(limits.fetch_timeout),
        total: limits.fetch_timeout,
    };
    let mut said = Vec::new();
    pipe::receive_up_to(
        &child_spec(child),
        &flags,
        lent.as_ref().map(AsFd::as_fd),
        caps,
        &mut said,
    )?;
    drop(lent);
    match Answer::parse(&said)? {
        Answer::Refused(why) => Err(DescribeFault::Refused(why).into()),
        Answer::Described(description) => {
            let cap = limits.max_component_bytes.min(MAX_COMPONENT_CEILING);
            Ok(description.select(commit, platform, cap)?)
        }
    }
}

#[cfg(test)]
mod tests;
