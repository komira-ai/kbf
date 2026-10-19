//! The files the flags name: there, regular, and the secrets private.

use std::io;
use std::path::Path;

use super::host::Kind;
use super::{Class, FileNeed, Host, Needs, Report};

/// Whether an error from the file system is one a restart may outlive: the same kinds
/// `kbf storage` calls not ready when its directories answer them (a busy or slow mount,
/// a read-only or full one). Everything else, a missing file and a refused permission
/// among them, needs an operator.
pub(super) fn class_of(e: &io::Error) -> Class {
    use io::ErrorKind::{
        Interrupted, ReadOnlyFilesystem, ResourceBusy, StorageFull, TimedOut, WouldBlock,
    };
    match e.kind() {
        TimedOut | ResourceBusy | Interrupted | WouldBlock | ReadOnlyFilesystem | StorageFull => {
            Class::NotReady
        }
        _ => Class::Refused,
    }
}

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    for f in &needs.files {
        one(f, host, report);
    }
}

fn one(f: &FileNeed, host: &dyn Host, report: &mut Report) {
    let FileNeed {
        flag,
        path,
        secret,
        exact_len,
    } = f;
    let path_text = path.display();
    let why = if *secret {
        "kbf cannot prove its identity without it"
    } else {
        "kbf reads it at start"
    };
    let stat = match host.stat(path) {
        Ok(stat) => stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.refuse(
                format!("{flag} {path_text} does not exist"),
                why,
                format!("create it, or point {flag} at the file that is there"),
            );
            return;
        }
        Err(e) => {
            report.push(
                class_of(&e),
                format!("{flag} {path_text} cannot be read: {e}"),
                why,
                "give the user kbf runs as search access to every directory on the path and \
                 read access to the file"
                    .to_owned(),
            );
            return;
        }
    };
    if stat.kind != Kind::File {
        report.refuse(
            format!("{flag} {path_text} is not a regular file"),
            why,
            format!("point {flag} at a regular file"),
        );
        return;
    }
    if !secret {
        return;
    }
    let me = host.euid();
    if stat.uid != me {
        report.refuse(
            format!(
                "{flag} {path_text} is owned by uid {}, not by uid {me}, the user kbf runs as",
                stat.uid
            ),
            "a secret another user owns is not private to the service",
            format!("chown {me} {path_text}"),
        );
    }
    if stat.mode & 0o400 == 0 {
        report.refuse(
            format!(
                "{flag} {path_text} has mode {:04o}, which the user kbf runs as cannot read",
                stat.mode
            ),
            why,
            format!("chmod 600 {path_text}"),
        );
        return;
    }
    if stat.mode & 0o077 != 0 {
        report.refuse(
            format!("{flag} {path_text} has mode {:04o}", stat.mode),
            "the group or others can read the secret",
            format!("chmod 600 {path_text}"),
        );
    }
    if let Some(len) = *exact_len
        && let Some(bytes) = read(host, flag, path, why, report)
        && bytes.len() != len
    {
        report.refuse(
            format!(
                "{flag} {path_text} holds {} bytes, not exactly {len}",
                bytes.len()
            ),
            "the server refuses to start on a secret of another size",
            format!(
                "write exactly {len} random bytes with no newline: \
                 head -c {len} /dev/urandom > {path_text}; chmod 600 {path_text}"
            ),
        );
    }
}

/// The bytes of the file `flag` names, or `None` with the read error in `report`: a file
/// that stats fine can still refuse the read (a root-owned 0600 file after a root-run
/// renewal), and the mode would then fail at its TLS load and be restarted for good. An
/// error on a path the files check already reported is not reported twice.
pub(super) fn read(
    host: &dyn Host,
    flag: &str,
    path: &Path,
    why: &str,
    report: &mut Report,
) -> Option<Vec<u8>> {
    let e = match host.read(path) {
        Ok(bytes) => return Some(bytes),
        Err(e) => e,
    };
    let at = path.display().to_string();
    if !report.problems.iter().any(|p| p.what.contains(&at)) {
        report.push(
            class_of(&e),
            format!("{flag} {at} cannot be read: {e}"),
            why,
            "give the user kbf runs read access to the file (a file renewed by root is \
             root's: chown it back)"
                .to_owned(),
        );
    }
    None
}
