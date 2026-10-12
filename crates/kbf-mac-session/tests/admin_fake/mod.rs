//! A fake Mac for the `node-admin` tests: a real directory tree as the Mac's `/`,
//! owners and access control lists from tables (a path nobody listed reads as
//! root's; modes, types and sizes are the real ones), `defaults`, `pmset` and
//! `sw_vers` over in-memory settings, `dscl .` and `dseditgroup`
//! over an in-memory directory service, `cron` and `at` files as a fresh Mac has them,
//! and a log of every program run.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the fake"
)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_mac_session::autologin::CommandOutput;
use kbf_mac_session::node_admin::{self, AdminHost, Meta, Sudo};

pub const SOFTWARE_UPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
pub const COMMERCE: &str = "/Library/Preferences/com.apple.commerce";

/// The fake clock: 2027-01-15T08:00:00Z.
pub const NOW: u64 = 1_800_000_000;

/// More pauses than any bounded wait takes: past this the test panics, not hangs.
const PAUSES: usize = 1_000;

/// What runs on the fake Mac at each pause: its number, from 1.
pub type PauseHook = Box<dyn FnMut(usize)>;

/// What runs on the fake Mac as each program starts: the command's text.
pub type CommandHook = Box<dyn FnMut(&str)>;

/// The keys `pmset -g` lists on a real Mac (macOS 26 on a Mac mini), in its order.
const PMSET_ORDER: [&str; 14] = [
    "standby",
    "Sleep On Power Button",
    "hibernatefile",
    "powernap",
    "networkoversleep",
    "disksleep",
    "sleep",
    "hibernatemode",
    "ttyskeepawake",
    "displaysleep",
    "tcpkeepalive",
    "lowpowermode",
    "womp",
    "autorestart",
];

pub struct State {
    pub euid: u32,
    /// Owners by path; any other path is root's.
    pub owners: BTreeMap<PathBuf, u32>,
    /// Paths that have an access control list.
    pub acls: BTreeSet<PathBuf>,
    /// Paths whose lstat fails.
    pub lstat_fails: BTreeSet<PathBuf>,
    /// What `current_exe` answers; an error when `None`.
    pub exe: Option<PathBuf>,
    /// Preferences by (domain, key): "0" or "1".
    pub prefs: BTreeMap<(String, String), String>,
    /// `pmset` settings by key.
    pub pmset: BTreeMap<String, String>,
    /// Every program run, as `program arg ...`.
    pub calls: Vec<String>,
    /// Commands (by the start of their text) that exit 1 with this standard error.
    pub fails: BTreeMap<String, String>,
    /// Commands (by the start of their text) that cannot be started.
    pub unstartable: BTreeSet<String>,
    pub pauses: usize,
    pub on_pause: Option<PauseHook>,
    pub on_command: Option<CommandHook>,
    pub now: SystemTime,
    /// Directory service records (`/Users/<name>`, `/Groups/<name>`) and their
    /// attributes.
    pub ds: BTreeMap<String, BTreeMap<String, String>>,
    /// The members of `admin`.
    pub admins: BTreeSet<String>,
    /// Groups by path, as `chown` set them.
    pub groups: BTreeMap<PathBuf, u32>,
    /// Every `chown`, as `<path> <uid>:<gid>`.
    pub chowns: Vec<String>,
    /// Paths whose `chown` fails.
    pub chown_fails: BTreeSet<PathBuf>,
}

pub struct FakeMac {
    pub root: PathBuf,
    pub state: RefCell<State>,
}

/// One call's result.
#[derive(Debug)]
pub struct Ran {
    pub code: u8,
    pub out: String,
    pub err: String,
}

impl Ran {
    /// Refused (exit 2), for a reason that holds `why`.
    #[track_caller]
    pub fn refused_for(&self, why: &str) {
        assert_eq!(self.code, 2, "{self:?}");
        assert!(self.err.starts_with("REFUSED: "), "{self:?}");
        assert!(
            self.err.contains(why),
            "want a refusal for {why:?}: {self:?}"
        );
    }

    /// Failed (exit 1), for a reason that holds `why`.
    #[track_caller]
    pub fn failed_for(&self, why: &str) {
        assert_eq!(self.code, 1, "{self:?}");
        assert!(self.err.starts_with("FAILED: "), "{self:?}");
        assert!(
            self.err.contains(why),
            "want a failure for {why:?}: {self:?}"
        );
    }

    #[track_caller]
    pub fn succeeded(&self) {
        assert_eq!((self.code, self.err.as_str()), (0, ""), "{self:?}");
    }

    #[track_caller]
    pub fn usage(&self) {
        assert_eq!(self.code, 2, "{self:?}");
        assert!(self.err.starts_with("usage: "), "{self:?}");
    }
}

fn dir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

impl FakeMac {
    /// A fresh Mac: the helper's tree as setup leaves it, the update settings on, the
    /// Mac sleeping after a minute and staying off after a power cut.
    pub fn new(name: &str) -> Self {
        // Files the tests make are not group-writable, whatever umask the runner has.
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("node-admin-{name}"));
        let _ = fs::remove_dir_all(&root);
        dir(&root, 0o755);
        let base = root.join("usr/local/kbf");
        for sub in ["sbin", "bin", "etc", "log"] {
            dir(&base.join(sub), 0o755);
        }
        dir(&base.join("state"), 0o700);
        for up in [root.join("usr"), root.join("usr/local"), base.clone()] {
            fs::set_permissions(&up, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let exe = base.join("sbin/kbf-mac-session");
        fs::write(&exe, "the helper").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        // As on a Mac: /private/var/at is daemon:wheel 0755, cron.deny lists Guest,
        // at.deny is empty.
        dir(&root.join("private/var/at"), 0o755);
        fs::write(root.join("private/var/at/cron.deny"), "Guest\n").unwrap();
        fs::write(root.join("private/var/at/at.deny"), "").unwrap();
        let owners = BTreeMap::from([(root.join("private/var/at"), 1)]);
        let record = |dir: &str, name: &str, key: &str, value: &str| {
            let attributes = BTreeMap::from([(key.to_owned(), value.to_owned())]);
            (format!("/{dir}/{name}"), attributes)
        };
        let ds = BTreeMap::from([
            record("Users", "root", "UniqueID", "0"),
            record("Users", "ops", "UniqueID", "501"),
            record("Groups", "wheel", "PrimaryGroupID", "0"),
            record("Groups", "staff", "PrimaryGroupID", "20"),
        ]);
        let prefs = [
            (SOFTWARE_UPDATE, "AutomaticDownload"),
            (SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates"),
            (SOFTWARE_UPDATE, "ConfigDataInstall"),
            (COMMERCE, "AutoUpdate"),
        ]
        .map(|(domain, key)| ((domain.to_owned(), key.to_owned()), "1".to_owned()));
        // Every key a real Mac lists, so a key matched by part of a line (`sleep` in
        // `disksleep`, `displaysleep` or `/var/vm/sleepimage`) reads the wrong value.
        let pmset = [
            ("standby", "1"),
            ("Sleep On Power Button", "1"),
            ("hibernatefile", "/var/vm/sleepimage"),
            ("powernap", "1"),
            ("networkoversleep", "0"),
            ("disksleep", "10"),
            ("sleep", "1"),
            ("hibernatemode", "3"),
            ("ttyskeepawake", "1"),
            ("displaysleep", "10"),
            ("tcpkeepalive", "1"),
            ("lowpowermode", "0"),
            ("womp", "1"),
            ("autorestart", "0"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        let state = State {
            euid: 0,
            owners,
            acls: BTreeSet::new(),
            lstat_fails: BTreeSet::new(),
            exe: Some(exe),
            prefs: prefs.into_iter().collect(),
            pmset: pmset.into_iter().collect(),
            calls: Vec::new(),
            fails: BTreeMap::new(),
            unstartable: BTreeSet::new(),
            pauses: 0,
            on_pause: None,
            on_command: None,
            now: UNIX_EPOCH + Duration::from_secs(NOW),
            ds,
            admins: BTreeSet::new(),
            groups: BTreeMap::new(),
            chowns: Vec::new(),
            chown_fails: BTreeSet::new(),
        };
        Self {
            root,
            state: RefCell::new(state),
        }
    }

    pub fn base(&self) -> PathBuf {
        self.root.join("usr/local/kbf")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.base().join("state")
    }

    pub fn record(&self) -> PathBuf {
        self.state_dir().join("phase0.before")
    }

    /// The role user's home.
    pub fn home(&self) -> PathBuf {
        self.base().join("home")
    }

    /// `/private/var/at`.
    pub fn at_dir(&self) -> PathBuf {
        self.root.join("private/var/at")
    }

    /// An attribute of a directory service record.
    pub fn attribute(&self, record: &str, name: &str) -> Option<String> {
        let state = self.state.borrow();
        state.ds.get(record)?.get(name).cloned()
    }

    /// Sets an attribute of the user's directory service record, making the record.
    pub fn set_user(&self, user: &str, name: &str, value: &str) {
        self.set_attribute(&format!("/Users/{user}"), name, value);
    }

    /// Sets an attribute of the group's directory service record, making the record.
    pub fn set_group(&self, group: &str, name: &str, value: &str) {
        self.set_attribute(&format!("/Groups/{group}"), name, value);
    }

    fn set_attribute(&self, record: &str, name: &str, value: &str) {
        let mut state = self.state.borrow_mut();
        let attributes = state.ds.entry(record.to_owned()).or_default();
        attributes.insert(name.to_owned(), value.to_owned());
    }

    /// Who owns `path`, and its group, as the fake says.
    pub fn owner(&self, path: &Path) -> (u32, u32) {
        let state = self.state.borrow();
        let uid = state.owners.get(path).copied().unwrap_or(0);
        (uid, state.groups.get(path).copied().unwrap_or(0))
    }

    pub fn admin_log(&self) -> PathBuf {
        self.base().join("log/admin.log")
    }

    pub fn lock(&self) -> PathBuf {
        self.state_dir().join("node-admin.lock")
    }

    /// The earlier phase-0 tool's record, holding `text`.
    pub fn earlier_record(&self, text: &str) -> PathBuf {
        let folder = self.root.join("Library/Application Support/kbf-mdm");
        for up in [
            self.root.join("Library"),
            self.root.join("Library/Application Support"),
        ] {
            dir(&up, 0o755);
        }
        dir(&folder, 0o700);
        let path = folder.join("phase0.before");
        fs::write(&path, text).unwrap();
        path
    }

    /// Who owns `path` from now on.
    pub fn own(&self, path: &Path, uid: u32) {
        self.state.borrow_mut().owners.insert(path.to_owned(), uid);
    }

    pub fn pref(&self, domain: &str, key: &str) -> Option<String> {
        let state = self.state.borrow();
        state
            .prefs
            .get(&(domain.to_owned(), key.to_owned()))
            .cloned()
    }

    pub fn pmset(&self, key: &str) -> String {
        self.state.borrow().pmset[key].clone()
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.borrow().calls.clone()
    }

    /// The calls that change a setting.
    pub fn changes(&self) -> Vec<String> {
        let changing = |call: &String| {
            call.contains(" write ") || call.contains(" delete ") || call.contains("pmset -a")
        };
        self.calls().into_iter().filter(changing).collect()
    }

    pub fn clear_calls(&self) {
        self.state.borrow_mut().calls.clear();
    }

    /// Runs `kbf-mac-session node-admin <args>` as sudo from uid 501 would.
    pub fn run(&self, args: &[&str]) -> Ran {
        let sudo = Sudo {
            user: Some("ops".into()),
            uid: Some("501".into()),
        };
        self.run_as(&sudo, &args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    pub fn run_as(&self, sudo: &Sudo, args: &[OsString]) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = node_admin::run(self, sudo, args, &mut out, &mut err);
        Ran {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn answer(&self, text: &str, program: &str, args: &[&str]) -> CommandOutput {
        let mut state = self.state.borrow_mut();
        if let Some((_, stderr)) = state
            .fails
            .iter()
            .find(|(start, _)| text.starts_with(*start))
        {
            return CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.clone(),
            };
        }
        let ok = |stdout: String| CommandOutput {
            success: true,
            stdout,
            stderr: String::new(),
        };
        let key = |domain: &str, key: &str| (domain.to_owned(), key.to_owned());
        match (program, args) {
            ("/usr/bin/defaults", ["read", domain, name]) => {
                match state.prefs.get(&key(domain, name)) {
                    Some(value) => ok(format!("{value}\n")),
                    None => CommandOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: format!(
                            "The domain/default pair of ({domain}, {name}) does not exist\n"
                        ),
                    },
                }
            }
            ("/usr/bin/defaults", ["write", domain, name, "-bool", value]) => {
                let value = if *value == "true" { "1" } else { "0" };
                state.prefs.insert(key(domain, name), value.to_owned());
                ok(String::new())
            }
            ("/usr/bin/defaults", ["delete", domain, name]) => {
                state.prefs.remove(&key(domain, name));
                ok(String::new())
            }
            ("/usr/bin/pmset", ["-g"]) => {
                // As a real Mac prints it: its own order (keys it does not know last),
                // names padded to 20 columns, and a note after a sleep that is held off.
                let known = PMSET_ORDER.iter().filter_map(|name| {
                    let value = state.pmset.get(*name)?;
                    Some((*name, value))
                });
                let unknown = state
                    .pmset
                    .iter()
                    .filter(|(name, _)| !PMSET_ORDER.contains(&name.as_str()))
                    .map(|(name, value)| (name.as_str(), value));
                let mut text = "System-wide power settings:\nCurrently in use:\n".to_owned();
                for (name, value) in known.chain(unknown) {
                    let note = if name == "sleep" && value != "0" {
                        " (sleep prevented by sharingd, powerd)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(" {name:<20} {value}{note}\n"));
                }
                ok(text)
            }
            ("/usr/bin/pmset", ["-a", name, value]) => {
                state.pmset.insert((*name).to_owned(), (*value).to_owned());
                ok(String::new())
            }
            ("/usr/bin/sw_vers", ["-productVersion"]) => ok("26.1\n".to_owned()),
            ("/usr/bin/sw_vers", ["-buildVersion"]) => ok("25B78\n".to_owned()),
            ("/usr/bin/dscl", [".", "-read", record, name]) => {
                // As dscl prints it: a value with a space in it on a line of its own.
                let found = state
                    .ds
                    .get(*record)
                    .map(|attributes| attributes.get(*name));
                match found {
                    None => CommandOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "<dscl_cmd> DS Error: -14136 (eDSRecordNotFound)\n".to_owned(),
                    },
                    Some(None) => CommandOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: format!("No such key: {name}\n"),
                    },
                    Some(Some(value)) if value.contains(' ') => ok(format!("{name}:\n {value}\n")),
                    Some(Some(value)) => ok(format!("{name}: {value}\n")),
                }
            }
            ("/usr/bin/dscl", [".", "-create", record]) => {
                state.ds.entry((*record).to_owned()).or_default();
                ok(String::new())
            }
            ("/usr/bin/dscl", [".", "-create", record, name, value]) => {
                let attributes = state.ds.entry((*record).to_owned()).or_default();
                attributes.insert((*name).to_owned(), (*value).to_owned());
                ok(String::new())
            }
            ("/usr/bin/dscl", [".", "-list", folder, name]) => {
                let prefix = format!("{folder}/");
                let mut text = String::new();
                for (record, attributes) in &state.ds {
                    if let (Some(short), Some(value)) =
                        (record.strip_prefix(&prefix), attributes.get(*name))
                    {
                        text.push_str(&format!("{short:<30} {value}\n"));
                    }
                }
                ok(text)
            }
            ("/usr/sbin/dseditgroup", ["-o", "checkmember", "-m", user, "admin"]) => {
                if state.admins.contains(*user) {
                    ok(format!("yes {user} is a member of admin\n"))
                } else {
                    CommandOutput {
                        success: false,
                        stdout: format!("no {user} is NOT a member of admin\n"),
                        stderr: String::new(),
                    }
                }
            }
            _ => panic!("the fake Mac has no answer for {text:?}"),
        }
    }
}

impl AdminHost for FakeMac {
    fn root(&self) -> &Path {
        &self.root
    }

    fn euid(&self) -> u32 {
        self.state.borrow().euid
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        let exe = self.state.borrow().exe.clone();
        exe.ok_or_else(|| io::Error::other("no executable path"))
    }

    fn lstat(&self, path: &Path) -> io::Result<Option<Meta>> {
        let state = self.state.borrow();
        if state.lstat_fails.contains(path) {
            return Err(io::Error::other("lstat failed"));
        }
        let meta = node_admin::lstat(path)?;
        Ok(meta.map(|meta| Meta {
            uid: state.owners.get(path).copied().unwrap_or(0),
            ..meta
        }))
    }

    fn has_acl(&self, path: &Path) -> io::Result<bool> {
        Ok(self.state.borrow().acls.contains(path))
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        assert!(
            program.starts_with('/'),
            "{program} is not an absolute path"
        );
        let text = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.state.borrow_mut().calls.push(text.clone());
        let hook = self.state.borrow_mut().on_command.take();
        if let Some(mut hook) = hook {
            hook(&text);
            self.state.borrow_mut().on_command = Some(hook);
        }
        let unstartable = self
            .state
            .borrow()
            .unstartable
            .iter()
            .any(|start| text.starts_with(start));
        if unstartable {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such program"));
        }
        Ok(self.answer(&text, program, args))
    }

    fn chown(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        if state.chown_fails.contains(path) {
            return Err(io::Error::other("chown failed"));
        }
        // lchown: the path must be there, and a link is never followed.
        fs::symlink_metadata(path)?;
        state.chowns.push(format!("{} {uid}:{gid}", path.display()));
        state.owners.insert(path.to_owned(), uid);
        state.groups.insert(path.to_owned(), gid);
        Ok(())
    }

    fn now(&self) -> SystemTime {
        self.state.borrow().now
    }

    fn pause(&self) {
        let (count, hook) = {
            let mut state = self.state.borrow_mut();
            state.pauses += 1;
            assert!(state.pauses < PAUSES, "a wait without a bound");
            (state.pauses, state.on_pause.take())
        };
        if let Some(mut hook) = hook {
            hook(count);
            self.state.borrow_mut().on_pause = Some(hook);
        }
    }
}
