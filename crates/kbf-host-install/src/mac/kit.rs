//! The kit tree: which release `current` and `previous` name, what version a release was
//! installed as, which release the running daemon executes.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use super::layout::{BINARY, DAEMON_LABEL, MacHost, VERSION_FILE};
use super::release::is_commit;
use super::tools;
use crate::request::Version;
use crate::runner::Runner;

/// The releases `current` and `previous` name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Links {
    pub current: Option<String>,
    pub previous: Option<String>,
}

/// The commit a link names: `releases/<commit>`, as a link relative to the kit. None when
/// there is no link.
fn read_link(path: &Path) -> Result<Option<String>, String> {
    let target = match fs::read_link(path) {
        Ok(target) => target,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    target
        .to_str()
        .and_then(|text| text.strip_prefix("releases/"))
        .filter(|commit| is_commit(commit))
        .map(|commit| Some(commit.to_owned()))
        .ok_or_else(|| format!("{} does not name a release", path.display()))
}

/// Both links.
///
/// # Errors
/// A link that exists and does not name `releases/<commit>`.
pub fn links(host: &MacHost) -> Result<Links, String> {
    Ok(Links {
        current: read_link(&host.current())?,
        previous: read_link(&host.previous())?,
    })
}

/// Point the link at `path` to `releases/<commit>`, or remove it for none. The link is made
/// aside and renamed over the old one, so there is no moment without it.
///
/// # Errors
/// The link could not be made or moved.
pub fn set_link(path: &Path, commit: Option<&str>) -> io::Result<()> {
    let Some(commit) = commit else {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let aside = path.with_file_name(format!(".{name}.new"));
    match fs::remove_file(&aside) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    symlink(format!("releases/{commit}"), &aside)?;
    fs::rename(&aside, path)
}

/// The version text release `commit` was installed as, if it has a usable one.
pub fn installed_version(host: &MacHost, commit: &str) -> Option<String> {
    let text = host
        .kit_trust
        .read(&host.release(commit).join(VERSION_FILE))
        .ok()?;
    let version = text.strip_suffix('\n')?;
    Version::new(version).ok().map(|_| version.to_owned())
}

/// The commit of the release the running daemon executes: the daemon's process (from
/// `launchctl print`) has the file `releases/<commit>/kbf` open as its program (from `lsof`,
/// which names the file the kernel mapped, links resolved). Not the link, and not the file
/// on disk: a daemon that was not restarted still runs the old release. None when any step
/// does not say. (`lsof` exits 1 when it only warns, with its listing intact, so its output
/// is read whatever its status; a listing with no such file says nothing.)
pub fn running_commit(host: &MacHost, runner: &dyn Runner) -> Option<String> {
    let limit = host.tuning.tool;
    let print = runner
        .run(&tools::print(DAEMON_LABEL, limit))
        .ok()
        .filter(crate::runner::Exec::success)?;
    let pid = tools::print_pid(&print.stdout_text())?;
    let open = runner.run(&tools::lsof_text(pid, limit)).ok()?;
    let text = open.stdout_text();
    tools::lsof_paths(&text)
        .into_iter()
        .find_map(|path| commit_of_binary(host, Path::new(path)))
}

/// `commit` when `path` is `<releases>/<commit>/kbf`.
fn commit_of_binary(host: &MacHost, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(host.releases()).ok()?;
    let mut parts = rest.iter();
    let commit = parts.next()?.to_str()?;
    let binary = parts.next()?;
    (binary == BINARY && parts.next().is_none() && is_commit(commit)).then(|| commit.to_owned())
}
