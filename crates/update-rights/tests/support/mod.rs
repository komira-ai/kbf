//! Shared by the tests: a valid declaration, a scratch directory and fake accounts.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use update_rights::{Declaration, EtcAccounts, Refusal};

/// A declaration that keeps every rule, shaped like the example of the design (the numbers and
/// the paths are the tierstore owner's).
pub const SAMPLE: &str = r#"
name = "tierstore"
binary = "tierstore"
signer = { repository = "komira-ai/kbf", workflow = "artifacts.yml" }
uid = "tierstore"
registration_ttl_secs = 300
health_ports = [9301, 9302]
verify_deadline_secs = 1600
promotion_ceiling_secs = 3600
cleanup_retries = 3
state_dir = "/var/lib/updater/hooks/tierstore"
boot = "restore"
min_version = "0.1.0"

[hook]
user = "tierstore-update"
group = "tierstore-update"
capabilities = []
read_paths = ["/usr/libexec/kbf", "/usr/sbin/nft", "/etc/tierstore", "/etc/updater.d/tierstore.toml"]
write_paths = []
units = ["tierstore@9301.service", "tierstore@9302.service"]
address_families = ["AF_UNIX", "AF_INET", "AF_INET6", "AF_NETLINK"]
environment = { }

[hook.step_capabilities]
prepare = ["CAP_NET_ADMIN"]
cutover = ["CAP_NET_ADMIN"]
verify = []
rollback = ["CAP_NET_ADMIN"]
cleanup = []

[hook.timeout_secs]
prepare = 600
cutover = 300
verify = 1000
rollback = 600
cleanup = 1300

[hook.params]
promotion_bound_secs = 900
"#;

/// `SAMPLE` with `from` written as `to`. Panics if `from` is not in it, so a test cannot pass by
/// changing nothing.
pub fn with(from: &str, to: &str) -> String {
    with_in(SAMPLE, from, to)
}

pub fn with_in(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from:?} is not in the text");
    text.replacen(from, to, 1)
}

pub fn sample() -> Declaration {
    Declaration::parse(SAMPLE).expect("the sample is a declaration")
}

/// The refusal of a text that must be refused.
pub fn refusal(text: &str) -> Refusal {
    Declaration::parse(text).expect_err("the text must be refused")
}

/// A directory under the target directory, unique to the test, removed when it is dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("update-rights-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("set the mode");
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A file of this mode, whatever the umask.
    pub fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        let _ = fs::remove_file(&path);
        fs::write(&path, text).expect("write the file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set the mode");
        path
    }

    /// A directory of this mode, whatever the umask.
    pub fn dir(&self, name: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).expect("create the directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set the mode");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn my_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn my_gid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// Accounts: root, the service and its hook account, and the others the tests name.
pub fn accounts() -> EtcAccounts {
    EtcAccounts::new(
        "root:x:0:0:root:/root:/bin/sh\n\
         tierstore:x:990:990::/nonexistent:/usr/sbin/nologin\n\
         tierstore-update:x:991:991::/nonexistent:/usr/sbin/nologin\n\
         other:x:992:992::/nonexistent:/usr/sbin/nologin\n\
         other-update:x:993:993::/nonexistent:/usr/sbin/nologin\n\
         toor:x:0:0:root by another name:/root:/bin/sh\n\
         tsb:x:995:995::/nonexistent:/usr/sbin/nologin\n\
         tsb-update:x:996:996::/nonexistent:/usr/sbin/nologin\n\
         alias:x:990:990::/nonexistent:/usr/sbin/nologin\n",
        "root:x:0:\n\
         tierstore:x:990:\n\
         tierstore-update:x:991:\n\
         other:x:992:\n\
         other-update:x:993:\n\
         toor:x:0:\n\
         tsb:x:995:\n\
         tsb-update:x:996:\n\
         alias:x:990:\n\
         shared:x:994:tierstore,other\n",
    )
}

/// A loaded declaration made by hand, for the tests that need one without a directory: its hook
/// account has the uid `hook_uid` and the host declares these `names` (the service's own among
/// them).
pub fn declared(
    declaration: Declaration,
    hook_uid: u32,
    names: &[&str],
) -> update_rights::Declared {
    update_rights::Declared {
        declaration,
        digest: update_rights::Digest::of(b"by hand"),
        file: PathBuf::from("by-hand.toml"),
        service: update_rights::UserIds { uid: 990, gid: 990 },
        hook: update_rights::UserIds {
            uid: hook_uid,
            gid: hook_uid,
        },
        service_gids: vec![990],
        names: names.iter().map(|name| (*name).to_owned()).collect(),
    }
}
