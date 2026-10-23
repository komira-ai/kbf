//! The programs a Mac install runs, as argument vectors, and the readers of what they print.
//!
//! Nothing here runs anything, and nothing is piped: a tool's whole output is captured by
//! the runner and matched in Rust.

use std::path::Path;
use std::time::Duration;

use crate::runner::{Cmd, Identity};

pub const LAUNCHCTL: &str = "/bin/launchctl";
pub const CODESIGN: &str = "/usr/bin/codesign";
pub const XATTR: &str = "/usr/bin/xattr";
pub const DSCL: &str = "/usr/bin/dscl";
pub const LSOF: &str = "/usr/sbin/lsof";
pub const SUDO: &str = "/usr/bin/sudo";

/// The extended attribute that marks a file as downloaded.
pub const QUARANTINE: &str = "com.apple.quarantine";

/// A proxy that refuses every connection: the attestation check is offline whatever the
/// host's network does.
const DEAD_PROXY: &str = "http://127.0.0.1:9";

fn cmd(program: &str, args: &[&str], limit: Duration) -> Cmd {
    Cmd {
        program: program.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout: Some(limit),
        ..Cmd::default()
    }
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

/// `launchctl print system/<label>`: succeeds only while the service is loaded.
pub fn print(label: &str, limit: Duration) -> Cmd {
    cmd(LAUNCHCTL, &["print", &format!("system/{label}")], limit)
}

/// `launchctl bootout system/<label>`: stops the service and unloads it. It returns when
/// the service has stopped, but the service may be listed a moment longer.
pub fn bootout(label: &str, limit: Duration) -> Cmd {
    cmd(LAUNCHCTL, &["bootout", &format!("system/{label}")], limit)
}

/// `launchctl bootstrap system <plist>`.
pub fn bootstrap(plist: &Path, limit: Duration) -> Cmd {
    Cmd {
        args: vec!["bootstrap".into(), "system".into(), path_arg(plist)],
        ..cmd(LAUNCHCTL, &[], limit)
    }
}

/// The value of a top-level field of a `launchctl print` listing: a line of one tab, then
/// `key = value`. The job's nested blocks (`arguments`, `endpoints`, `event triggers`, ...)
/// are indented further and carry fields of their own, `state` among them; they are never
/// read.
fn print_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let field = line.strip_prefix('\t')?;
        if field.starts_with(char::is_whitespace) {
            return None;
        }
        let (name, value) = field.split_once(" = ")?;
        (name == key).then_some(value.trim())
    })
}

/// The process id of the running service in a `launchctl print` listing.
pub fn print_pid(text: &str) -> Option<u32> {
    print_value(text, "pid")?
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
}

/// Whether a `launchctl print` listing says the service is running.
pub fn print_running(text: &str) -> bool {
    print_value(text, "state") == Some("running")
}

/// Whether a `launchctl print` listing says the service ran and ended: it is not running and
/// has a last exit code (or a terminating signal). A job that is loaded and has not started
/// yet (`spawn scheduled`, `(never exited)`) is not finished.
pub fn print_finished(text: &str) -> bool {
    !print_running(text)
        && (print_value(text, "last exit code").is_some_and(|code| code.parse::<i32>().is_ok())
            || print_value(text, "last terminating signal").is_some())
}

/// The files `pid` has open as its program and libraries, one `n<path>` field each.
pub fn lsof_text(pid: u32, limit: Duration) -> Cmd {
    cmd(
        LSOF,
        &["-nPw", "-a", "-p", &pid.to_string(), "-d", "txt", "-F", "n"],
        limit,
    )
}

/// The paths in `lsof -F n` output, in order.
pub fn lsof_paths(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.strip_prefix('n'))
        .collect()
}

/// The attributes of `path`, one name a line.
pub fn xattr_list(path: &Path, limit: Duration) -> Cmd {
    Cmd {
        args: vec![path_arg(path)],
        ..cmd(XATTR, &[], limit)
    }
}

/// Remove the quarantine attribute of `path`.
pub fn xattr_remove_quarantine(path: &Path, limit: Duration) -> Cmd {
    Cmd {
        args: vec!["-d".into(), QUARANTINE.into(), path_arg(path)],
        ..cmd(XATTR, &[], limit)
    }
}

/// Whether an attribute listing names the quarantine attribute.
pub fn has_quarantine(listing: &str) -> bool {
    listing.lines().any(|line| line.trim() == QUARANTINE)
}

/// `codesign --verify --strict`: the signature is valid and whole.
pub fn codesign_verify(path: &Path, limit: Duration) -> Cmd {
    Cmd {
        args: vec!["--verify".into(), "--strict".into(), path_arg(path)],
        ..cmd(CODESIGN, &[], limit)
    }
}

/// `codesign -d --verbose=4`: describes the signature on stderr.
pub fn codesign_describe(path: &Path, limit: Duration) -> Cmd {
    Cmd {
        args: vec!["-d".into(), "--verbose=4".into(), path_arg(path)],
        ..cmd(CODESIGN, &[], limit)
    }
}

/// Whether a `codesign -d` description has the hardened runtime: the code directory's
/// `flags=0x...(a,b,...)` lists `runtime`.
pub fn has_hardened_runtime(description: &str) -> bool {
    description.lines().any(|line| {
        line.split_whitespace()
            .filter_map(|word| word.strip_prefix("flags="))
            .filter_map(|flags| flags.split_once('(')?.1.strip_suffix(')'))
            .any(|names| names.split(',').any(|name| name == "runtime"))
    })
}

/// The ids of the account `name`.
pub fn dscl_ids(name: &str, limit: Duration) -> Cmd {
    cmd(
        DSCL,
        &[
            ".",
            "-read",
            &format!("/Users/{name}"),
            "UniqueID",
            "PrimaryGroupID",
        ],
        limit,
    )
}

/// The user and group ids in `dscl -read` output (`UniqueID: 4294967294`).
pub fn parse_ids(text: &str) -> Option<Identity> {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?
            .trim()
            .parse::<u32>()
            .ok()
    };
    Some(Identity {
        uid: value("UniqueID")?,
        gid: value("PrimaryGroupID")?,
    })
}

/// `gh attestation verify <tarball> --bundle <bundle> ...`, offline, as `who`, in the home
/// directory `home`. The arguments are the ones the installer passes.
pub struct Attest<'a> {
    pub gh: &'a Path,
    pub tarball: &'a Path,
    pub bundle: &'a Path,
    pub trusted_root: &'a Path,
    pub repo: &'a str,
    pub workflow: &'a str,
    pub source_ref: &'a str,
    pub commit: &'a str,
    pub who: Identity,
    pub home: &'a Path,
}

/// The issuer of GitHub Actions' OIDC tokens, which signs the attestation's certificate.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The attestation predicate: SLSA build provenance.
pub const PREDICATE: &str = "https://slsa.dev/provenance/v1";

impl Attest<'_> {
    /// The command.
    pub fn cmd(&self, limit: Duration) -> Cmd {
        let mut env: Vec<(String, String)> = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ]
        .iter()
        .map(|name| ((*name).to_owned(), DEAD_PROXY.to_owned()))
        .collect();
        env.push(("HOME".into(), path_arg(self.home)));
        env.push(("GH_CONFIG_DIR".into(), path_arg(self.home)));
        env.push(("GH_NO_UPDATE_NOTIFIER".into(), "1".into()));
        let mut args: Vec<String> = vec!["attestation".into(), "verify".into()];
        args.push(path_arg(self.tarball));
        args.push("--bundle".into());
        args.push(path_arg(self.bundle));
        args.push("--custom-trusted-root".into());
        args.push(path_arg(self.trusted_root));
        for (flag, value) in [
            ("--repo", self.repo),
            ("--signer-workflow", self.workflow),
            ("--cert-oidc-issuer", ISSUER),
            ("--source-ref", self.source_ref),
            ("--source-digest", self.commit),
            ("--predicate-type", PREDICATE),
        ] {
            args.push(flag.into());
            args.push(value.into());
        }
        // The tarball is hashed by `gh` itself: its sha256 must be a subject of the statement.
        args.push("--digest-alg".into());
        args.push("sha256".into());
        args.push("--deny-self-hosted-runners".into());
        Cmd {
            program: path_arg(self.gh),
            args,
            timeout: Some(limit),
            env,
            run_as: Some(self.who),
            ..Cmd::default()
        }
    }
}

/// The new binary checking the daemon's live configuration, as the daemon's user.
pub fn config_check(
    binary: &Path,
    config: &Path,
    home: &Path,
    who: Identity,
    limit: Duration,
) -> Cmd {
    Cmd {
        program: path_arg(binary),
        args: vec![
            "--mode=daemon".into(),
            "--config".into(),
            path_arg(config),
            "--check".into(),
        ],
        timeout: Some(limit),
        env: vec![("HOME".into(), path_arg(home))],
        run_as: Some(who),
        ..Cmd::default()
    }
}

/// This program again as root through sudo, with the request on its standard input. The
/// sudoers rule names exactly this command line.
pub fn sudo_root(self_exe: &Path, request: &str) -> Cmd {
    Cmd {
        program: SUDO.to_owned(),
        args: vec!["-n".into(), path_arg(self_exe), "--as-root".into()],
        stdin: request.as_bytes().to_vec(),
        ..Cmd::default()
    }
}
