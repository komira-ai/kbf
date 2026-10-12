//! The `node-admin` mode on a Mac: the real [`AdminHost`] and the mode's `main`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use crate::autologin::{self, CommandOutput};
use crate::node_admin::{self, AdminHost, Meta, Sudo};

/// `ls -lde` prints one line for a path, and one more per access control entry.
const LS: &str = "/bin/ls";

/// The Mac the helper runs on.
#[derive(Debug)]
pub struct MacAdmin {
    root: PathBuf,
}

impl Default for MacAdmin {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }
}

impl AdminHost for MacAdmin {
    fn root(&self) -> &Path {
        &self.root
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        std::env::current_exe()
    }

    fn lstat(&self, path: &Path) -> io::Result<Option<Meta>> {
        node_admin::lstat(path)
    }

    fn has_acl(&self, path: &Path) -> io::Result<bool> {
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path is not UTF-8"))?;
        let listed = autologin::run_command(LS, &["-lde", path])?;
        if !listed.success {
            return Err(io::Error::other(format!(
                "ls -lde failed: {}",
                listed.stderr.trim()
            )));
        }
        Ok(listed.stdout.lines().count() > 1)
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        autologin::run_command(program, args)
    }

    fn chown(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn pause(&self) {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Runs one `node-admin` call; `args` are the arguments after the mode.
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    // Files the mode makes get their modes explicitly; nothing it makes is more open.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let sudo = Sudo {
        user: std::env::var_os("SUDO_USER"),
        uid: std::env::var_os("SUDO_UID"),
    };
    let code = node_admin::run(
        &MacAdmin::default(),
        &sudo,
        args,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/kms-admin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Catches: an ACL check that never sees an access control entry, so a folder an
    /// ACL lets others write passes as root's alone.
    #[test]
    fn has_acl_sees_an_access_control_entry() {
        let dir = scratch("acl");
        let host = MacAdmin::default();
        assert!(!host.has_acl(&dir).unwrap());
        let added = autologin::run_command(
            "/bin/chmod",
            &["+a", "everyone allow add_file", dir.to_str().unwrap()],
        )
        .unwrap();
        assert!(added.success, "{added:?}");
        assert!(host.has_acl(&dir).unwrap());
        assert!(host.has_acl(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches: a chown that does not reach the OS (one that succeeds where the OS
    /// refuses, or never fails on a missing path).
    #[test]
    fn chown_changes_the_owner_through_the_os() {
        let dir = scratch("chown");
        let host = MacAdmin::default();
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        let (uid, gid) = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        host.chown(&file, uid, gid).unwrap();
        assert!(host.chown(&dir.join("missing"), uid, gid).is_err());
        if uid != 0 {
            assert!(
                host.chown(&file, 0, 0).is_err(),
                "only root gives a file away"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches: a host whose lstat follows links or whose root is not `/`.
    #[test]
    fn the_mac_host_looks_at_the_real_tree() {
        let host = MacAdmin::default();
        assert_eq!(host.root(), Path::new("/"));
        let dir = scratch("lstat");
        std::os::unix::fs::symlink(&dir, dir.join("link")).unwrap();
        let link = host.lstat(&dir.join("link")).unwrap().unwrap();
        assert_eq!(link.kind, node_admin::Kind::Link);
        assert_eq!(host.lstat(&dir.join("missing")).unwrap(), None);
        assert_eq!(host.euid(), rustix::process::geteuid().as_raw());
        assert!(host.current_exe().unwrap().is_absolute());
        let output = host
            .command("/usr/bin/sw_vers", &["-productVersion"])
            .unwrap();
        assert!(output.success, "{output:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
