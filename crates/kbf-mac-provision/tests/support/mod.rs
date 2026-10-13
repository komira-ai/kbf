//! A fake Mac for the tests: each macOS program the checks run is answered from
//! state in memory, in the output format of macOS's own; the files are a real scratch
//! directory, the fake's `/`; owners can be set per path, since the tests do not run
//! as root. Every program call is logged.

#![allow(dead_code, reason = "each test binary uses part of the fake")]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_mac_provision::host::{Host, Output, Stat};
use kbf_mac_provision::items::Report;
use sha2::{Digest as _, Sha256};

mod golden;
mod pool;
pub use golden::{golden_plist, golden_plist_as};

/// The example profile the tests start from.
pub const EXAMPLE: &str = include_str!("../../../../tools/mac-provision/example.profile");

pub const PLIST: &str = "/Library/LaunchDaemons/org.example.kbf-daemon.plist";
pub const DROPIN: &str = "/etc/ssh/sshd_config.d/000-kbf.conf";
pub const ERR_LOG: &str = "/Library/Logs/kbf/kbf-daemon.err.log";
pub const SOFTWARE_UPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
pub const COMMERCE: &str = "/Library/Preferences/com.apple.commerce";
pub const LOGINWINDOW: &str = "/Library/Preferences/com.apple.loginwindow";
pub const INTENT: &str = "/var/db/kbf-mac-session/session-intent";
/// The example's volume (the default `kbf_volume`), its pool homes and the node key.
pub const VOLUME: &str = "/Volumes/kbf";
pub const HOMES: &str = "/Volumes/kbf/home";
pub const KEY: &str = "/usr/local/kbf/etc/node.key";
pub const SCRATCH: &str = "/Volumes/kbf/scratch";
/// The example's pool: 4 users (the default) from uid 700 (the default: the uid after
/// `lease_uids=600-699`); group `kbfpool` has gid 700.
pub const POOL_FIRST_UID: u32 = 700;

/// A fresh scratch directory for test `name`, unique within the test binary (tests run
/// in parallel and may share a name).
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-mac-provision-{}", env!("CARGO_CRATE_NAME")))
        .join(format!("{name}-{n}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The program state of a fake Mac.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub euid: u32,
    pub sw_vers: BTreeMap<&'static str, String>,
    /// `pmset -g`'s settings, in its order.
    pub pmset: Vec<(String, String)>,
    pub scutil: BTreeMap<String, String>,
    /// `defaults` values by domain (a path on the Mac) and key.
    pub defaults: BTreeMap<(String, String), String>,
    pub remote_login: bool,
    pub network_time: bool,
    pub time_server: String,
    /// What `socketfilterfw --getglobalstate` says: `1`, `0`, or anything else.
    pub firewall: String,
    /// The word `fdesetup status` puts in `FileVault is <word>.`.
    pub filevault: String,
    /// The offset `sntp -t` prints, or `fail`.
    pub sntp_offset: String,
    /// Directory records by path (`/Groups/admin`), attributes in order.
    pub records: BTreeMap<String, Vec<(String, String)>>,
    /// What `sshd -T` prints; `None` prints the drop-in's lines in lower case.
    pub sshd_effective: Option<String>,
    /// Loaded launchd jobs, as `system/<label>`.
    pub loaded: BTreeSet<String>,
    pub xcode_select: Option<String>,
    /// Owners `lstat` reports for paths on the Mac, over the file's real one.
    pub owners: BTreeMap<String, (u32, u32)>,
    /// `launchctl bootstrap` fails, as for a job that will not load.
    pub bootstrap_fails: bool,
    /// Calls that change the Mac (as the log shows them, paths on the Mac) which exit 1
    /// and change nothing when they start with one of these, as a `pmset -a` or a
    /// `defaults write` refused by the system.
    pub fails: Vec<String>,
    /// Calls that, starting with one of these, exit 0 and change nothing.
    pub no_effect: Vec<String>,
    /// Mounted volumes `diskutil info` reports: mount point, file system personality.
    pub volumes: BTreeMap<String, String>,
    /// Mount points `mdutil -s` reports indexing disabled for.
    pub spotlight_off: BTreeSet<String>,
    /// Paths `tmutil isexcluded` reports excluded.
    pub backup_excluded: BTreeSet<String>,
}

impl State {
    pub fn attr(&mut self, record: &str, attr: &str, value: &str) {
        let attrs = self.records.entry(record.into()).or_default();
        attrs.retain(|(a, _)| a != attr);
        attrs.push((attr.into(), value.into()));
    }

    /// An attribute of the user record `name`.
    pub fn user_attr(&mut self, name: &str, attr: &str, value: &str) {
        self.attr(&format!("/Users/{name}"), attr, value);
    }

    pub fn remove_user(&mut self, name: &str) {
        self.records.remove(&format!("/Users/{name}"));
    }

    pub fn user(&mut self, name: &str, uid: u32) {
        self.attr(&format!("/Users/{name}"), "UniqueID", &uid.to_string());
    }

    pub fn pmset_set(&mut self, key: &str, value: &str) {
        match self.pmset.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value.into(),
            None => self.pmset.push((key.into(), value.into())),
        }
    }

    pub fn default_set(&mut self, domain: &str, key: &str, value: &str) {
        self.defaults
            .insert((domain.into(), key.into()), value.into());
    }
}

/// A fake Mac: [`State`] plus a scratch directory as its `/`.
pub struct FakeMac {
    pub root: String,
    /// The programs' state; the programs `apply` runs change it.
    pub state: RefCell<State>,
    pub log: RefCell<Vec<String>>,
    /// The test profile, made when the Mac is: a drift of the files it pins (an Xcode
    /// `.xip`, the probe) does not change it.
    profile: String,
}

impl FakeMac {
    /// A Mac as Setup Assistant and Remote Login leave it: one administrator, nothing
    /// else set; the two Xcode `.xip`s and the probe script are in place.
    pub fn fresh(name: &str) -> Self {
        let root = scratch(name);
        for d in [
            "etc/ssh/sshd_config.d",
            "Applications",
            "Library/kbf/xip",
            "Library/Preferences",
            "usr/local/kbf/probes",
            "var/db/kbf-mac-session",
            "Volumes/kbf",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut state = State {
            euid: 0,
            remote_login: true,
            network_time: true,
            time_server: "time.apple.com".into(),
            firewall: "0".into(),
            filevault: "Off".into(),
            sntp_offset: "+0.004000".into(),
            ..State::default()
        };
        state.sw_vers.insert("version", "27.0.1".into());
        state.sw_vers.insert("build", "26A000".into());
        for (k, v) in [
            ("sleep", "1"),
            ("standby", "1"),
            ("powernap", "1"),
            ("autorestart", "0"),
            ("womp", "0"),
        ] {
            state.pmset_set(k, v);
        }
        state
            .scutil
            .insert("ComputerName".into(), "Farm Admin Mac".into());
        state
            .scutil
            .insert("LocalHostName".into(), "Farm-Admin-Mac".into());
        state.user("root", 0);
        state.user("farmadmin", 501);
        state.attr("/Groups/admin", "GroupMembership", "root farmadmin");
        // The operator makes the volume at provisioning (diskutil apfs addVolume).
        state.volumes.insert(VOLUME.into(), "APFS".into());
        let mac = Self {
            root: root.to_str().unwrap().to_owned(),
            state: RefCell::new(state),
            log: RefCell::new(Vec::new()),
            profile: String::new(),
        };
        mac.write(
            "/etc/ssh/sshd_config",
            "# sshd_config\nInclude /etc/ssh/sshd_config.d/*\n",
        );
        mac.write(
            "/Library/kbf/xip/Xcode_26.6.xip",
            "app=Xcode-26.6.app build=17A000\n",
        );
        mac.write(
            "/Library/kbf/xip/Xcode_27.0.xip",
            "app=Xcode-27.0.app build=18A000\n",
        );
        mac.write("/usr/local/kbf/probes/host_identity.sh", "echo identity\n");
        let mut mac = mac;
        mac.profile = mac.make_profile();
        mac
    }

    /// A fresh Mac with every item of [`Self::profile`] as `apply` leaves it.
    pub fn converged(name: &str) -> Self {
        let mut mac = Self::fresh(name);
        let s = mac.state.get_mut();
        for n in ["HostName", "LocalHostName", "ComputerName"] {
            s.scutil.insert(n.into(), "kbf-mac-07".into());
        }
        for (a, v) in [
            ("UniqueID", "480"),
            ("PrimaryGroupID", "480"),
            ("UserShell", "/usr/bin/false"),
            ("NFSHomeDirectory", "/var/empty"),
            ("IsHidden", "1"),
            ("Password", "*"),
        ] {
            s.user_attr("_kbf", a, v);
        }
        s.attr("/Groups/_kbf", "PrimaryGroupID", "480");
        s.attr("/Groups/_kbf", "Password", "*");
        for (k, v) in [
            ("sleep", "0"),
            ("standby", "0"),
            ("powernap", "0"),
            ("autorestart", "1"),
            ("womp", "1"),
        ] {
            s.pmset_set(k, v);
        }
        for (k, v) in [
            ("AutomaticDownload", "0"),
            ("AutomaticallyInstallMacOSUpdates", "0"),
            ("CriticalUpdateInstall", "0"),
            ("ConfigDataInstall", "1"),
            ("AutomaticallyInstallAppUpdates", "0"),
        ] {
            s.default_set(SOFTWARE_UPDATE, k, v);
        }
        s.default_set(COMMERCE, "AutoUpdate", "0");
        s.firewall = "1".into();
        s.time_server = "time.example.net".into();
        s.xcode_select = Some("/Applications/Xcode-27.0.app/Contents/Developer".into());
        s.loaded.insert("system/org.example.kbf-daemon".into());
        mac.write(
            DROPIN,
            "PasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin no\n",
        );
        mac.chmod(DROPIN, 0o644);
        mac.own(DROPIN, 0, 0);
        mac.install_xcode("Xcode-26.6.app", "17A000");
        mac.install_xcode("Xcode-27.0.app", "18A000");
        let profile = mac.profile_text();
        mac.write(PLIST, &golden_plist(&sha256_hex(profile.as_bytes())[..12]));
        mac.chmod(PLIST, 0o644);
        mac.own(PLIST, 0, 0);
        mac.mkdir("/Library/Logs/kbf");
        mac.chmod("/Library/Logs/kbf", 0o755);
        mac.own("/Library/Logs/kbf", 0, 0);
        mac.write(ERR_LOG, "");
        mac.chmod(ERR_LOG, 0o644);
        mac.own(ERR_LOG, 480, 480);
        mac.converge_volume();
        mac.converge_pool(4, POOL_FIRST_UID);
        mac
    }

    /// Where `path` on the Mac is on this machine.
    pub fn path(&self, path: &str) -> PathBuf {
        PathBuf::from(format!("{}{path}", self.root))
    }

    pub fn write(&self, path: &str, text: &str) {
        let p = self.path(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    pub fn mkdir(&self, path: &str) {
        fs::create_dir_all(self.path(path)).unwrap();
    }

    pub fn remove(&self, path: &str) {
        let p = self.path(path);
        if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(p).unwrap();
        } else {
            fs::remove_file(p).unwrap();
        }
    }

    pub fn chmod(&self, path: &str, mode: u32) {
        fs::set_permissions(self.path(path), fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The programs' state, to set up a test.
    pub fn state_mut(&mut self) -> &mut State {
        self.state.get_mut()
    }

    pub fn own(&mut self, path: &str, uid: u32, gid: u32) {
        self.state.get_mut().owners.insert(path.into(), (uid, gid));
    }

    /// A symbolic link at `path` to `target` (a path on this machine).
    pub fn symlink(&self, target: &Path, path: &str) {
        std::os::unix::fs::symlink(target, self.path(path)).unwrap();
    }

    pub fn install_xcode(&self, app: &str, build: &str) {
        self.write(
            &format!("/Applications/{app}/Contents/version.plist"),
            &format!("ProductBuildVersion={build}\n"),
        );
    }

    /// The test profile: the example with the fake's own Xcode and probe SHA-256s.
    pub fn profile_text(&self) -> String {
        self.profile.clone()
    }

    fn make_profile(&self) -> String {
        let mut text: String = EXAMPLE
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("xcode=") && !l.starts_with("probe="))
            .map(|l| {
                if l.starts_with("profile=") {
                    "profile=mac-arm64-test\n".to_owned()
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
        let file_sha = |p: &str| sha256_hex(&fs::read(self.path(p)).unwrap());
        text.push_str(&format!(
            "xcode=17A000:/Applications/Xcode-26.6.app/Contents/Developer:{}\n",
            file_sha("/Library/kbf/xip/Xcode_26.6.xip")
        ));
        text.push_str(&format!(
            "xcode=18A000:/Applications/Xcode-27.0.app/Contents/Developer:{}\n",
            file_sha("/Library/kbf/xip/Xcode_27.0.xip")
        ));
        text.push_str(&format!(
            "probe=host_identity:/usr/local/kbf/probes/host_identity.sh:{}\n",
            file_sha("/usr/local/kbf/probes/host_identity.sh")
        ));
        text
    }

    /// Writes `text` as the profile file (outside the fake's `/`) and returns its path.
    pub fn profile_file(&self, text: &str) -> PathBuf {
        let p = PathBuf::from(format!("{}.profile", self.root));
        fs::write(&p, text).unwrap();
        p
    }

    /// The calls that would change a Mac, from the log: none of them is a read.
    pub fn writes(&self) -> Vec<String> {
        const READS: &[&str] = &[
            "sw_vers -",
            "scutil --get ",
            "pmset -g",
            "defaults read ",
            "dscl . -read ",
            "dscl . -list ",
            "dseditgroup -o checkmember ",
            "systemsetup -get",
            "socketfilterfw --getglobalstate",
            "fdesetup status",
            "sntp -t ",
            "sshd -T",
            "launchctl print ",
            "xcode-select -p",
            "PlistBuddy -c Print :ProductBuildVersion ",
            "diskutil info ",
            "mdutil -s ",
            "tmutil isexcluded ",
        ];
        self.log
            .borrow()
            .iter()
            .filter(|call| !READS.iter().any(|r| call.starts_with(r)))
            .cloned()
            .collect()
    }

    fn strip_root<'a>(&self, path: &'a str) -> &'a str {
        path.strip_prefix(self.root.as_str()).unwrap_or(path)
    }

    /// The answer of a program that changes the Mac, having changed the fake's state;
    /// `None` if the fake has none for these arguments. `dir` is the directory it runs
    /// in, a path on the Mac.
    fn change(&self, name: &str, args: &[&str], dir: Option<&str>) -> Option<(bool, String)> {
        let mut s = self.state.borrow_mut();
        let done = |success: bool| Some((success, String::new()));
        let bit = |word: &str| {
            if word == "true" || word == "on" {
                "1"
            } else {
                "0"
            }
        };
        let shown: Vec<&str> = args.iter().map(|a| self.strip_root(a)).collect();
        let call = format!("{name} {}", shown.join(" "));
        let starts = |prefixes: &[String]| prefixes.iter().any(|p| call.starts_with(p.as_str()));
        if starts(&s.fails) {
            return done(false);
        }
        if starts(&s.no_effect) {
            return done(true);
        }
        match (name, args) {
            ("pmset", ["-a", key, value]) => {
                s.pmset_set(key, value);
                done(true)
            }
            ("defaults", ["write", domain, key, "-bool", value @ ("true" | "false")]) => {
                let domain = self.strip_root(domain).to_owned();
                s.default_set(&domain, key, bit(value));
                done(true)
            }
            ("defaults", ["delete", domain, key]) => {
                let at = (self.strip_root(domain).to_owned(), (*key).to_owned());
                done(s.defaults.remove(&at).is_some())
            }
            ("scutil", ["--set", n, value]) => {
                s.scutil.insert((*n).into(), (*value).into());
                done(true)
            }
            ("systemsetup", ["-f", "-setremotelogin", state @ ("on" | "off")]) => {
                s.remote_login = *state == "on";
                done(true)
            }
            ("systemsetup", ["-setusingnetworktime", state @ ("on" | "off")]) => {
                s.network_time = *state == "on";
                done(true)
            }
            ("systemsetup", ["-setnetworktimeserver", server]) => {
                s.time_server = (*server).into();
                done(true)
            }
            ("socketfilterfw", ["--setglobalstate", state @ ("on" | "off")]) => {
                s.firewall = bit(state).into();
                done(true)
            }
            ("dscl", [".", "-create", record, rest @ ..]) => {
                s.records.entry((*record).into()).or_default();
                match rest {
                    [] => done(true),
                    [attr, value] => {
                        s.attr(record, attr, value);
                        done(true)
                    }
                    _ => None,
                }
            }
            ("dscl", [".", "-delete", record, attr]) => {
                let attrs = s.records.get_mut(*record)?;
                let before = attrs.len();
                attrs.retain(|(a, _)| a != attr);
                done(attrs.len() != before)
            }
            ("launchctl", ["bootout", domain]) => done(s.loaded.remove(*domain)),
            // The label is read from the property list, as launchd reads it.
            ("launchctl", ["bootstrap", "system", plist]) => {
                let text = fs::read_to_string(plist).unwrap_or_default();
                let label = text
                    .split("<key>Label</key>\n\t<string>")
                    .nth(1)
                    .and_then(|rest| rest.split("</string>").next())
                    .unwrap_or_default();
                let job = format!("system/{label}");
                if s.bootstrap_fails || label.is_empty() || s.loaded.contains(&job) {
                    done(false)
                } else {
                    s.loaded.insert(job);
                    done(true)
                }
            }
            ("xcode-select", ["-s", dir]) => {
                s.xcode_select = Some((*dir).into());
                done(true)
            }
            ("sntp", ["-sS", _]) => {
                s.sntp_offset = "+0.000100".into();
                done(true)
            }
            // A fake .xip holds the line "app=NAME.app build=BUILD"; it expands into the
            // directory it runs in.
            ("xip", ["--expand", file]) => {
                let text = fs::read_to_string(file).ok()?;
                let line = text.lines().next()?;
                let (app, build) = line.strip_prefix("app=")?.split_once(" build=")?;
                let plist = self.path(&format!("{}/{app}/Contents/version.plist", dir?));
                fs::create_dir_all(plist.parent()?).ok()?;
                fs::write(plist, format!("ProductBuildVersion={build}\n")).ok()?;
                done(true)
            }
            ("xcodebuild", ["-license", "accept"] | ["-runFirstLaunch"]) => done(true),
            ("mdutil", ["-i", "off", volume]) => {
                s.spotlight_off.insert(self.strip_root(volume).into());
                done(true)
            }
            ("tmutil", ["addexclusion", path]) => {
                s.backup_excluded.insert(self.strip_root(path).into());
                done(true)
            }
            ("dseditgroup", ["-o", "edit", "-d", user, "-t", "user", group]) => {
                let record = format!("/Groups/{group}");
                let members: Vec<String> = s
                    .records
                    .get(&record)
                    .and_then(|a| a.iter().find(|(k, _)| k == "GroupMembership"))
                    .map_or_else(Vec::new, |(_, v)| {
                        v.split_whitespace()
                            .filter(|m| m != user)
                            .map(str::to_owned)
                            .collect()
                    });
                s.attr(&record, "GroupMembership", &members.join(" "));
                done(true)
            }
            _ => None,
        }
    }

    /// The answer of a program that only reads, or `None`.
    fn answer(&self, name: &str, args: &[&str]) -> Option<(bool, String)> {
        let s = self.state.borrow();
        let ok = |text: String| Some((true, text));
        let no = |text: &str| Some((false, text.to_owned()));
        match (name, args) {
            ("sw_vers", ["-productVersion"]) => ok(format!("{}\n", s.sw_vers["version"])),
            ("sw_vers", ["-buildVersion"]) => ok(format!("{}\n", s.sw_vers["build"])),
            ("scutil", ["--get", n]) => match s.scutil.get(*n) {
                Some(v) => ok(format!("{v}\n")),
                None => no(&format!("{n}: not set\n")),
            },
            ("pmset", ["-g"]) => {
                let mut out = String::from(
                    "System-wide power settings:\nCurrently in use:\n Sleep On Power Button 1\n",
                );
                for (k, v) in &s.pmset {
                    out.push_str(&format!(" {k:<20} {v}\n"));
                }
                ok(out)
            }
            ("defaults", ["read", domain, key]) => {
                match s
                    .defaults
                    .get(&(self.strip_root(domain).to_owned(), (*key).to_owned()))
                {
                    Some(v) => ok(format!("{v}\n")),
                    None => no(""),
                }
            }
            ("dscl", [".", "-read", record, attrs @ ..]) => {
                let Some(found) = s.records.get(*record) else {
                    return no("");
                };
                let mut out = String::new();
                for a in attrs {
                    if let Some((_, v)) = found.iter().find(|(k, _)| k == a) {
                        match *a {
                            // A long value goes on a line of its own, as macOS's dscl prints it.
                            "AuthenticationAuthority" => out.push_str(&format!("{a}:\n {v}\n")),
                            "IsHidden" => out.push_str(&format!("dsAttrTypeNative:{a}: {v}\n")),
                            _ => out.push_str(&format!("{a}: {v}\n")),
                        }
                    }
                }
                ok(out)
            }
            ("dscl", [".", "-list", dir, rest @ ..]) => {
                let prefix = format!("{dir}/");
                let mut out = String::new();
                for (record, attrs) in &s.records {
                    let Some(name) = record.strip_prefix(&prefix) else {
                        continue;
                    };
                    match rest {
                        [attr] => {
                            let v = attrs
                                .iter()
                                .find(|(k, _)| k == attr)
                                .map_or("", |(_, v)| v.as_str());
                            out.push_str(&format!("{name:<24} {v}\n"));
                        }
                        _ => out.push_str(&format!("{name}\n")),
                    }
                }
                ok(out)
            }
            // A member is listed in the group's GroupMembership or has the group as its
            // primary group (the fake knows one group id: admin is 80).
            ("dseditgroup", ["-o", "checkmember", "-m", user, group]) => {
                let get = |record: String, attr: &str| {
                    s.records
                        .get(&record)
                        .and_then(|a| a.iter().find(|(k, _)| k == attr))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let listed = get(format!("/Groups/{group}"), "GroupMembership")
                    .split_whitespace()
                    .any(|m| m == *user);
                let primary =
                    *group == "admin" && get(format!("/Users/{user}"), "PrimaryGroupID") == "80";
                if listed || primary {
                    ok(format!("yes {user} is a member of {group}\n"))
                } else {
                    no(&format!("no {user} is NOT a member of {group}\n"))
                }
            }
            ("systemsetup", ["-getremotelogin"]) => ok(format!(
                "Remote Login: {}\n",
                if s.remote_login { "On" } else { "Off" }
            )),
            ("systemsetup", ["-getusingnetworktime"]) => ok(format!(
                "Network Time: {}\n",
                if s.network_time { "On" } else { "Off" }
            )),
            ("systemsetup", ["-getnetworktimeserver"]) => {
                ok(format!("Network Time Server: {}\n", s.time_server))
            }
            ("socketfilterfw", ["--getglobalstate"]) => ok(match s.firewall.as_str() {
                "1" => "Firewall is enabled. (State = 1)\n".into(),
                "0" => "Firewall is disabled. (State = 0)\n".into(),
                _ => "socketfilterfw: no answer\n".into(),
            }),
            ("fdesetup", ["status"]) => ok(format!("FileVault is {}.\n", s.filevault)),
            // As macOS's diskutil prints a mounted volume's information, in part.
            ("diskutil", ["info", volume]) => match s.volumes.get(*volume) {
                Some(personality) => ok(format!(
                    "   Device Identifier:         disk3s7\n   Mount Point:               {volume}\n   File System Personality:   {personality}\n"
                )),
                None => no(&format!("Could not find disk: {volume}\n")),
            },
            ("mdutil", ["-s", volume]) => ok(format!(
                "{volume}:\n\tIndexing {}.\n",
                if s.spotlight_off.contains(*volume) {
                    "disabled"
                } else {
                    "enabled"
                }
            )),
            ("tmutil", ["isexcluded", path]) => ok(format!(
                "[{}]    {path}\n",
                if s.backup_excluded.contains(*path) {
                    "Excluded"
                } else {
                    "Included"
                }
            )),
            ("sntp", ["-t", "5", server]) => {
                if s.sntp_offset == "fail" {
                    no("")
                } else {
                    ok(format!(
                        "{} +/- 0.010000 {server} 192.0.2.1\n",
                        s.sntp_offset
                    ))
                }
            }
            ("sshd", ["-T"]) => match &s.sshd_effective {
                Some(text) => ok(text.clone()),
                None => ok(fs::read_to_string(self.path(DROPIN))
                    .unwrap_or_default()
                    .lines()
                    .map(|l| {
                        let (k, v) = l.split_once(' ').unwrap_or((l, ""));
                        format!("{} {v}\n", k.to_lowercase())
                    })
                    .collect()),
            },
            ("launchctl", ["print", domain]) => {
                if s.loaded.contains(*domain) {
                    ok(String::new())
                } else {
                    no("")
                }
            }
            ("xcode-select", ["-p"]) => match &s.xcode_select {
                Some(dir) => ok(format!("{dir}\n")),
                None => no(""),
            },
            // Only `Print :ProductBuildVersion` of a fake version.plist (KEY=VALUE lines).
            ("PlistBuddy", ["-c", "Print :ProductBuildVersion", file]) => {
                let text = fs::read_to_string(file).unwrap_or_default();
                match text
                    .lines()
                    .find_map(|l| l.strip_prefix("ProductBuildVersion="))
                {
                    Some(v) => ok(format!("{v}\n")),
                    None => no(""),
                }
            }
            _ => None,
        }
    }
}

// The file methods change the scratch directory, never through a link, and log each
// change. They are called through `Host` (`Host::write(&mac, ..)`): `FakeMac`'s own
// `write`, `mkdir`, `chmod` and `remove` are the tests' setup helpers, which log
// nothing.
impl Host for FakeMac {
    fn root(&self) -> &str {
        &self.root
    }

    fn euid(&self) -> u32 {
        self.state.borrow().euid
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        dir: Option<&str>,
        env: &[(&str, &str)],
    ) -> io::Result<Output> {
        let name = program.rsplit('/').next().unwrap();
        let env: String = env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        self.log
            .borrow_mut()
            .push(format!("{env}{name} {}", args.join(" ")));
        let answer = match dir {
            None => self.answer(name, args),
            Some(_) => None,
        }
        .or_else(|| self.change(name, args, dir));
        match answer {
            Some((success, stdout)) => Ok(Output { success, stdout }),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no fake for {program} {args:?}"),
            )),
        }
    }

    fn lstat(&self, path: &str) -> Option<Stat> {
        let mut stat = Stat::of(&fs::symlink_metadata(self.path(path)).ok()?);
        if let Some(&(uid, gid)) = self.state.borrow().owners.get(path) {
            stat.uid = uid;
            stat.gid = gid;
        }
        Some(stat)
    }

    fn write(&self, path: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
        self.log.borrow_mut().push(format!("write {path}"));
        let p = self.path(path);
        if p.is_symlink() {
            return Err(io::Error::other("a link"));
        }
        let new = !p.exists();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&p)?;
        file.write_all(bytes)?;
        if new {
            fs::set_permissions(&p, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    fn mkdir(&self, path: &str) -> io::Result<()> {
        self.log.borrow_mut().push(format!("mkdir {path}"));
        fs::create_dir_all(self.path(path))
    }

    fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
        self.log.borrow_mut().push(format!("chmod {mode:o} {path}"));
        fs::set_permissions(self.path(path), fs::Permissions::from_mode(mode))
    }

    /// Records the owner `lstat` reports from now on (the tests do not run as root).
    fn chown(&self, path: &str, uid: u32, gid: u32) -> io::Result<()> {
        self.log
            .borrow_mut()
            .push(format!("chown {uid}:{gid} {path}"));
        fs::symlink_metadata(self.path(path))?;
        self.state
            .borrow_mut()
            .owners
            .insert(path.into(), (uid, gid));
        Ok(())
    }

    /// The owner recorded for `from` moves with it, and what was recorded for `to` goes.
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.log.borrow_mut().push(format!("mv {from} {to}"));
        fs::rename(self.path(from), self.path(to))?;
        let mut s = self.state.borrow_mut();
        s.owners.remove(to);
        if let Some(owner) = s.owners.remove(from) {
            s.owners.insert(to.into(), owner);
        }
        Ok(())
    }

    /// Removes the path and the owners recorded for it and anything under it.
    fn remove(&self, path: &str) -> io::Result<()> {
        self.log.borrow_mut().push(format!("rm {path}"));
        let p = self.path(path);
        match fs::symlink_metadata(&p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
            Ok(m) if m.is_dir() => fs::remove_dir_all(&p)?,
            Ok(_) => fs::remove_file(&p)?,
        }
        let under = format!("{path}/");
        self.state
            .borrow_mut()
            .owners
            .retain(|k, _| k != path && !k.starts_with(&under));
        Ok(())
    }

    fn pause(&self, seconds: u64) {
        self.log.borrow_mut().push(format!("sleep {seconds}"));
    }
}

pub fn check_keys(mac: &FakeMac, keys: Option<&str>) -> Report {
    let path = mac.profile_file(&mac.profile_text());
    kbf_mac_provision::cli::check(&path, keys, mac).unwrap()
}

pub fn check(mac: &FakeMac) -> Report {
    check_keys(mac, None)
}

/// The lines of a report that start with `prefix`.
pub fn lines<'a>(r: &'a Report, prefix: &str) -> Vec<&'a str> {
    r.text.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// On a converged Mac changed by `drift`, check fails `item` and nothing else, and
/// says `text`. `Err` says what went wrong instead.
pub fn only_fails(
    name: &str,
    item: &str,
    drift: fn(&mut FakeMac),
    text: &str,
) -> Result<(), String> {
    let mut mac = FakeMac::converged(name);
    drift(&mut mac);
    let r = check(&mac);
    let failed = lines(&r, "FAIL");
    let ok = r.failed == 1
        && failed.len() == 1
        && failed[0].starts_with(&format!("FAIL {item}: "))
        && failed[0].contains(text);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name}: want only FAIL {item}: ..{text}.., got:\n{}",
            r.text
        ))
    }
}

pub fn assert_only_fails(item: &str, drift: fn(&mut FakeMac), text: &str) {
    if let Err(e) = only_fails(&format!("only-{item}"), item, drift, text) {
        panic!("{e}");
    }
}

pub fn apply_keys(mac: &FakeMac, keys: Option<&str>) -> Report {
    let path = mac.profile_file(&mac.profile_text());
    kbf_mac_provision::cli::apply(&path, keys, mac).unwrap()
}

pub fn apply(mac: &FakeMac) -> Report {
    apply_keys(mac, None)
}

/// The value `pmset -g` reports for `key` on the fake.
pub fn pmset_value(mac: &FakeMac, key: &str) -> Option<String> {
    let s = mac.state.borrow();
    s.pmset
        .iter()
        .find_map(|(k, v)| (k == key).then(|| v.clone()))
}

/// The fake's `defaults` value of `domain` and `key`.
pub fn preference(mac: &FakeMac, domain: &str, key: &str) -> Option<String> {
    mac.state
        .borrow()
        .defaults
        .get(&(domain.to_owned(), key.to_owned()))
        .cloned()
}

/// `lstat` of `path` on the fake, as `ls -ldn`'s fields.
pub fn ls(mac: &FakeMac, path: &str) -> String {
    kbf_mac_provision::host::Host::lstat(mac, path).map_or_else(|| "missing".into(), |s| s.ls())
}

/// The program calls in the log that start with `prefix`.
pub fn calls(mac: &FakeMac, prefix: &str) -> Vec<String> {
    mac.log
        .borrow()
        .iter()
        .filter(|c| c.starts_with(prefix))
        .cloned()
        .collect()
}
