//! A scratch host for the package verbs' tests: a real directory tree under a scratch
//! root, with the system tools (`useradd`, `usermod`, `loginctl`, `systemctl`, ...)
//! played by a small model that changes the same tree and its own tables, so a verb run
//! twice sees what the first run did.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;

use kbf_host::path::Root;
use kbf_host::plan::Mode;
use kbf_host::step::Owner;
use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::user::{GETENT, ID};

use super::*;
use crate::linux::host::SYSTEMCTL;

/// The commit the package under test ships.
pub const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Another release's commit.
pub const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The ids a scratch test can chown to: its own.
pub fn me() -> Owner {
    Owner {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

/// The `usermod` flags the model knows: the flag, the file it changes, and whether it adds.
const USERMOD_FLAGS: [(&str, &str, bool); 4] = [
    ("--add-subuids", "/etc/subuid", true),
    ("--add-subgids", "/etc/subgid", true),
    ("--del-subuids", "/etc/subuid", false),
    ("--del-subgids", "/etc/subgid", false),
];

/// What the tools of the fake host know.
pub struct World {
    pub users: BTreeMap<String, (u32, u32)>,
    pub groups: BTreeSet<String>,
    pub enabled: BTreeSet<String>,
    pub active: BTreeSet<String>,
    /// What `systemctl show --property=NeedDaemonReload` says.
    pub need_reload: bool,
    /// `crun --version`'s exit code; `None`: no such program.
    pub crun: Option<i32>,
    /// Whether `groupadd` makes the group (when it does not, `useradd --gid` exits 6).
    pub groupadd_creates: bool,
    /// Whether `useradd` makes the user (a host where it exits 0 and does not is broken).
    pub useradd_creates: bool,
    /// Whether `userdel` also removes the group of the same name (`USERGROUPS_ENAB`).
    pub userdel_removes_group: bool,
    pub root: PathBuf,
}

pub struct Fixture {
    pub exec: ScratchHost,
    pub world: Rc<RefCell<World>>,
    pub env: Env,
}

/// Write `bytes` to `path` under the root with `mode`.
pub fn write(exec: &ScratchHost, path: &str, bytes: &str, mode: u32) {
    let full = exec.at(path);
    fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
    let _ = fs::remove_file(&full);
    fs::write(&full, bytes).expect("write");
    fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
}

pub fn read(exec: &ScratchHost, path: &str) -> String {
    fs::read_to_string(exec.at(path)).unwrap_or_default()
}

pub fn exists(exec: &ScratchHost, path: &str) -> bool {
    fs::symlink_metadata(exec.at(path)).is_ok()
}

/// The mode of `path`, without the file type.
pub fn mode_of(exec: &ScratchHost, path: &str) -> u32 {
    fs::symlink_metadata(exec.at(path))
        .expect("there")
        .permissions()
        .mode()
        & 0o7777
}

/// The target of the link at `path`.
pub fn link_of(exec: &ScratchHost, path: &str) -> String {
    fs::read_link(exec.at(path))
        .expect("a link")
        .display()
        .to_string()
}

/// A package's files: the release and the trust files, as dpkg would unpack them.
pub fn ship(exec: &ScratchHost, commit: &str, binary: &str) {
    write(exec, "/usr/lib/kbf/release/kbf", binary, 0o755);
    write(
        exec,
        "/usr/lib/kbf/release/manifest.json",
        &format!("{{\"commit\": \"{commit}\", \"version\": \"0.1\"}}\n"),
        0o644,
    );
    write(
        exec,
        "/usr/lib/kbf/release/units/kbf-node.service",
        &format!("# {binary}\n"),
        0o644,
    );
    write(exec, "/usr/lib/kbf/libexec/gh", "gh-1", 0o755);
    write(
        exec,
        "/usr/lib/kbf/libexec/trusted_root.jsonl",
        "root-1",
        0o644,
    );
    write(exec, "/usr/lib/kbf/libexec/installer.conf", "conf-1", 0o640);
}

/// A host the package can be installed on: systemd running, `crun` and the setuid
/// `newuidmap` and `newgidmap` there, the package's files unpacked, and no `kbf` account.
pub fn fixture(name: &str) -> Fixture {
    let mut exec = ScratchHost::new(name);
    let root = exec.root.clone();
    let world = Rc::new(RefCell::new(World {
        users: BTreeMap::new(),
        groups: BTreeSet::new(),
        enabled: BTreeSet::new(),
        active: BTreeSet::new(),
        need_reload: false,
        crun: Some(0),
        groupadd_creates: true,
        useradd_creates: true,
        userdel_removes_group: true,
        root: root.clone(),
    }));
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let ok = || Ok(reply(0, "", ""));
        match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", name]) => Ok(match w.users.get(*name) {
                Some((u, g)) => reply(
                    0,
                    &format!("{name}:x:{u}:{g}::/var/lib/{name}:/bin/false\n"),
                    "",
                ),
                None => reply(2, "", ""),
            }),
            (GETENT, ["group", "--", name]) => {
                Ok(reply(if w.groups.contains(*name) { 0 } else { 2 }, "", ""))
            }
            (ID, ["-G", "--", name]) => Ok(reply(0, &format!("{}\n", w.users[*name].1), "")),
            (CRUN, ["--version"]) => match w.crun {
                Some(code) => Ok(reply(code, "crun version 1.8\n", "")),
                None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            },
            (GROUPADD, ["--system", name]) => {
                if w.groupadd_creates {
                    w.groups.insert((*name).to_owned());
                }
                ok()
            }
            (GROUPDEL, [name]) => {
                w.groups.remove(*name);
                ok()
            }
            (USERADD, ["--system", "--gid", group, rest @ .., name]) => {
                // The flags the package passes; a group that is not there is useradd's exit 6.
                assert_eq!(
                    rest,
                    [
                        "--home-dir",
                        "/var/lib/kbf",
                        "--no-create-home",
                        "--shell",
                        "/usr/sbin/nologin",
                        "--comment",
                        "kbf build farm node"
                    ]
                );
                if !w.groups.contains(*group) {
                    return Ok(reply(6, "", "useradd: group does not exist"));
                }
                let m = me();
                if w.useradd_creates {
                    w.users.insert((*name).to_owned(), (m.uid, m.gid));
                }
                ok()
            }
            (USERDEL, [name]) => {
                w.users.remove(*name);
                if w.userdel_removes_group {
                    w.groups.remove(*name);
                }
                ok()
            }
            (USERMOD, flags) => {
                let (name, flags) = flags.split_last().expect("a user");
                for pair in flags.chunks(2) {
                    let (flag, span) = (pair[0], pair[1]);
                    let (_, file, add) = *USERMOD_FLAGS
                        .iter()
                        .find(|(f, _, _)| *f == flag)
                        .expect("a usermod flag the model knows");
                    let (first, last) = span.split_once('-').expect("first-last");
                    let (first, last): (u64, u64) = (first.parse().unwrap(), last.parse().unwrap());
                    let line = format!("{name}:{first}:{}", last - first + 1);
                    let full = w.root.join(file.trim_start_matches('/'));
                    let text = fs::read_to_string(&full).unwrap_or_default();
                    let mut lines: Vec<&str> = text.lines().collect();
                    if add {
                        lines.push(&line);
                    } else {
                        lines.retain(|l| *l != line);
                    }
                    let mut text = lines.join("\n");
                    text.push('\n');
                    fs::write(&full, text).expect("write");
                }
                ok()
            }
            (LOGINCTL, ["enable-linger", name]) => {
                let full = w.root.join(format!("var/lib/systemd/linger/{name}"));
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(full, "").unwrap();
                ok()
            }
            (LOGINCTL, ["disable-linger", name]) => {
                let _ = fs::remove_file(w.root.join(format!("var/lib/systemd/linger/{name}")));
                ok()
            }
            (SYSTEMCTL, ["show", "--property=NeedDaemonReload", "--value", UNIT]) => {
                Ok(reply(0, if w.need_reload { "yes\n" } else { "no\n" }, ""))
            }
            (SYSTEMCTL, ["daemon-reload"]) => {
                w.need_reload = false;
                ok()
            }
            (SYSTEMCTL, ["is-active", "--quiet", u]) => {
                Ok(reply(i32::from(!w.active.contains(*u)), "", ""))
            }
            (SYSTEMCTL, ["is-enabled", "--quiet", u]) => {
                Ok(reply(i32::from(!w.enabled.contains(*u)), "", ""))
            }
            (SYSTEMCTL, ["enable", u]) => {
                w.enabled.insert((*u).to_owned());
                ok()
            }
            (SYSTEMCTL, ["stop", u]) => {
                w.active.remove(*u);
                ok()
            }
            other => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the fake host does not know {other:?}"),
            )),
        }
    });
    for dir in [
        "usr/local",
        "var/lib",
        "run/systemd/system",
        "etc",
        "usr/bin",
    ] {
        fs::create_dir_all(exec.at(dir)).expect("dir");
    }
    write(&exec, NEWUIDMAP, "", 0o4755);
    write(&exec, NEWGIDMAP, "", 0o4755);
    ship(&exec, COMMIT, "kbf-1");
    let env = Env {
        root: Root::new(&root).expect("absolute"),
        root_owner: me(),
    };
    Fixture { exec, world, env }
}

impl Fixture {
    /// Run `postinst` in `mode`; the output and the result.
    pub fn postinst(&mut self, mode: Mode) -> (String, Result<(), PackageError>) {
        let mut out = Vec::new();
        let r = postinst::run(&mut self.exec, &self.env, mode, &mut out);
        (String::from_utf8(out).expect("utf-8"), r)
    }

    /// The steps run so far, as printed.
    pub fn steps(&self) -> Vec<String> {
        self.exec.log.iter().map(ToString::to_string).collect()
    }

    /// Put `kbf` in the tables, as a package left it.
    pub fn with_kbf(&self) {
        let m = me();
        let mut w = self.world.borrow_mut();
        w.groups.insert(USER.to_owned());
        w.users.insert(USER.to_owned(), (m.uid, m.gid));
    }

    /// `/etc/subuid` and `/etc/subgid` hold `text`.
    pub fn subids(&self, text: &str) {
        write(&self.exec, "/etc/subuid", text, 0o644);
        write(&self.exec, "/etc/subgid", text, 0o644);
    }
}
