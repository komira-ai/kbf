//! The plan `setup` carries out: the accounts, the directories, the token and `kbf.toml`, then
//! the units. Order matters and is the design's (distribution.md section 4): everything a
//! service needs comes first, `kbf.toml` is the last file written (a crash before it leaves
//! no config that passes the units' `ConditionPathExists`), and units are enabled last.
//!
//! Every step is added only when the host differs, so a second run with the same flags has an
//! empty plan.

use kbf_host::{Cmd, Entry, Exec, HostPath, Kind, Owner, Perms, Plan, Planner, RunUser};

use super::config::KbfToml;
use super::os::{DSCL, LAUNCHCTL, Os, SYSTEMCTL};
use super::validate::{Role, Valid};
use super::{SetupError, Start};
use crate::layout::Platform;

const ETC_DIR: &str = "/etc/kbf";
pub const KBF_TOML: &str = "/etc/kbf/kbf.toml";
pub const TOKEN: &str = "/etc/kbf/enroll.token";
/// The group of the `kbf` process: it reads the token, nobody else but root does.
const SERVICE_USER: &str = "kbf";
const USERADD: &str = "/usr/sbin/useradd";
const USERMOD: &str = "/usr/sbin/usermod";
const LOGINCTL: &str = "/usr/bin/loginctl";
/// The subid range a new grant starts at, and its size (what `usermod` documents).
const SUBID_FLOOR: u64 = 100_000;
const SUBID_COUNT: u64 = 65_536;
/// The first id setup considers for the macOS fetch account (below 500 is a system account).
const MAC_ID_FROM: u32 = 300;
const MAC_ID_TO: u32 = 400;
/// The label of the macOS daemon, and where the package puts its plist.
pub const MAC_LABEL: &str = "com.komira.kbf-daemon";

fn hp(path: &str) -> Result<HostPath, SetupError> {
    Ok(HostPath::new(path)?)
}

/// The directory the installer keeps its own state in: root's alone, `orders/` inside it is the
/// service user's.
pub fn installer_dir(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "/var/lib/kbf-installer",
        Platform::MacOs => "/var/db/kbf-installer",
    }
}

/// `current/kbf` under the releases directory: what a unit runs, so what must exist before one
/// is started.
pub fn release_binary(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "/var/lib/kbf/releases/current/kbf",
        Platform::MacOs => "/var/db/kbf/releases/current/kbf",
    }
}

fn fetch_user(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "kbf-fetch",
        Platform::MacOs => "_kbf-fetch",
    }
}

/// The units a role is run by, and whether they serve the frontend. The names are the static
/// unit files of the package (packaging/linux/units); the installer's own service is listed
/// apart, since it must run whatever the role.
pub const INSTALLER_UNIT: &str = "kbf-installer.service";

pub fn role_units(role: Role) -> &'static [&'static str] {
    match role {
        Role::Frontend => &["kbf-envoy.service", "kbf-frontend@a.service"],
        Role::Storage => &["kbf-storage.service"],
        Role::Daemon => &["kbf-daemon.service"],
        Role::StorageAndDaemon => &["kbf-storage-and-daemon.service"],
        Role::AllInOne => &["kbf-all-in-one.service"],
    }
}

/// The plan and what to tell the operator.
pub struct Outcome {
    pub plan: Plan,
    pub notes: Vec<String>,
}

/// A token file's content as setup keeps it: one line, a newline after it.
fn read_token<E: Exec>(exec: &E, file: &str) -> Result<Vec<u8>, SetupError> {
    let bad = |why: &str| SetupError::Token(format!("{file}: {why}"));
    let path = hp(file)?;
    let meta = exec
        .stat(&path)?
        .ok_or_else(|| bad("no such file"))?;
    if meta.kind != Kind::File {
        return Err(bad("not a regular file"));
    }
    if meta.perms.bits() & 0o077 != 0 {
        return Err(bad(&format!(
            "mode {} lets others read the token: chmod 600 {file}",
            meta.perms
        )));
    }
    if meta.len > 4096 {
        return Err(bad("larger than 4 KiB, which no token is"));
    }
    let Some(Entry::File { bytes, .. }) = exec.inspect(&path)? else {
        return Err(bad("not a regular file"));
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| bad("not text"))?.trim();
    if text.is_empty() {
        return Err(bad("empty"));
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("holds more than one word: a token is a single line"));
    }
    Ok(format!("{text}\n").into_bytes())
}

/// The first `/etc/subuid`-style range for `user` is there (keyed by name or uid).
fn has_range(text: &str, user: &RunUser) -> bool {
    text.lines().any(|line| {
        let mut f = line.split(':');
        let key = f.next();
        let count = f.nth(1).and_then(|c| c.parse::<u64>().ok());
        (key == Some(user.name.as_str()) || key == Some(user.uid.to_string().as_str()))
            && count.is_some_and(|c| c > 0)
    })
}

/// Where a new range may start: above every range in `text` (so it overlaps no other user's).
fn next_range_start(text: &str) -> u64 {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(':').skip(1);
            let start = f.next()?.parse::<u64>().ok()?;
            let count = f.next()?.parse::<u64>().ok()?;
            Some(start.saturating_add(count))
        })
        .fold(SUBID_FLOOR, u64::max)
}

fn add_subids(
    p: &mut Planner<'_, impl Exec>,
    user: &RunUser,
    kind: &'static str,
    text: &str,
) -> Result<(), SetupError> {
    if has_range(text, user) {
        return Ok(());
    }
    let start = next_range_start(text);
    let end = start + SUBID_COUNT - 1;
    if end > u64::from(u32::MAX) {
        return Err(SetupError::SubIdSpace(kind));
    }
    p.run(
        Cmd::new(hp(USERMOD)?)
            .arg(format!("--add-{kind}s"))
            .arg(format!("{start}-{end}"))
            .arg("--")
            .arg(&user.name),
    );
    Ok(())
}

/// The lowest id in 300..400 that no user and no group on the host has.
fn free_mac_id(users: &str, groups: &str) -> Result<u32, SetupError> {
    let taken = |id: u32| {
        [users, groups].iter().any(|t| {
            t.lines()
                .any(|l| l.split_whitespace().last() == Some(id.to_string().as_str()))
        })
    };
    (MAC_ID_FROM..MAC_ID_TO)
        .find(|id| !taken(*id))
        .ok_or(SetupError::NoFreeId)
}

fn create_fetch_user(
    p: &mut Planner<'_, impl Exec>,
    os: &impl Os,
    name: &str,
) -> Result<(), SetupError> {
    match os.platform() {
        Platform::Linux => p.run(
            Cmd::new(hp(USERADD)?)
                .arg("--system")
                .arg("--user-group")
                .arg("--no-create-home")
                .arg("--home-dir")
                .arg("/nonexistent")
                .arg("--shell")
                .arg("/usr/sbin/nologin")
                .arg("--")
                .arg(name),
        ),
        Platform::MacOs => {
            let id = free_mac_id(
                &os.dscl_list("/Users UniqueID")?,
                &os.dscl_list("/Groups PrimaryGroupID")?,
            )?
            .to_string();
            let dscl = |record: &str, attr: &str, value: &str| -> Result<Cmd, SetupError> {
                Ok(Cmd::new(hp(DSCL)?)
                    .arg(".")
                    .arg("-create")
                    .arg(record)
                    .arg(attr)
                    .arg(value))
            };
            let (group, user) = (format!("/Groups/{name}"), format!("/Users/{name}"));
            for cmd in [
                dscl(&group, "PrimaryGroupID", &id)?,
                dscl(&user, "UniqueID", &id)?,
                dscl(&user, "PrimaryGroupID", &id)?,
                dscl(&user, "UserShell", "/usr/bin/false")?,
                dscl(&user, "NFSHomeDirectory", "/var/empty")?,
                dscl(&user, "IsHidden", "1")?,
            ] {
                p.run(cmd);
            }
        }
    }
    Ok(())
}

/// Build the plan for `v` on this host. Reads the host, changes nothing.
pub fn build<O: Os, E: Exec>(
    os: &O,
    exec: &E,
    v: &Valid,
    start: Start,
) -> Result<Outcome, SetupError> {
    let platform = os.platform();
    if platform == Platform::MacOs && v.role != Role::Daemon {
        return Err(SetupError::Unsupported(format!(
            "role {}: a Mac is a node only (--role daemon)",
            v.role
        )));
    }
    let token = read_token(exec, &v.token_file)?;
    let service = match platform {
        Platform::Linux => Some(os.user(SERVICE_USER)?.ok_or(SetupError::NoUser {
            name: SERVICE_USER,
            fix: "install the kbf-installer package, which creates it (sysusers)",
        })?),
        Platform::MacOs => None,
    };
    let mut p = Planner::new(exec);
    let mut notes = Vec::new();

    // Accounts.
    let fetch = fetch_user(platform);
    if os.user(fetch)?.is_none() {
        create_fetch_user(&mut p, os, fetch)?;
    }
    if let (Some(user), true) = (&service, v.role.runs_actions()) {
        add_subids(&mut p, user, "subuid", &os.subuid()?)?;
        add_subids(&mut p, user, "subgid", &os.subgid()?)?;
        if !os.lingers(&user.name)? {
            p.run(Cmd::new(hp(LOGINCTL)?).arg("enable-linger").arg(&user.name));
        }
    }

    // Directories. The installer's own tree is root's; `orders/` is the service user's (on a
    // Mac the daemon is root, so it is root's).
    let root = os.root_owner();
    let orders_owner = service
        .as_ref()
        .map_or(root, |u| Owner { uid: u.uid, gid: u.gid });
    let state = installer_dir(platform);
    p.dir(hp(ETC_DIR)?, Perms::new(0o755), Some(root))?;
    p.dir(hp(state)?, Perms::new(0o755), Some(root))?;
    p.dir(hp(&format!("{state}/orders"))?, Perms::new(0o700), Some(orders_owner))?;

    // The token, then the config last.
    let (token_perms, token_owner) = match &service {
        Some(u) => (0o640, Owner { uid: root.uid, gid: u.gid }),
        None => (0o600, root),
    };
    p.file(hp(TOKEN)?, Perms::new(token_perms), Some(token_owner), token)?;
    let text = KbfToml {
        role: v.role,
        frontend_url: &v.frontend_url,
        token_file: TOKEN,
        source: &v.source,
        floor: &v.floor,
    }
    .render();
    let existed = exec.stat(&hp(KBF_TOML)?)?.is_some();
    let changed = p.file(hp(KBF_TOML)?, Perms::new(0o644), Some(root), text.into_bytes())?;
    if changed && existed {
        notes.push(
            "kbf.toml changed: a running kbf keeps the old one until it is restarted; setup does not restart it"
                .to_owned(),
        );
    }

    // Units, last.
    let released = exec.stat(&hp(release_binary(platform))?)?.is_some();
    units(&mut p, os, v, start, released, &mut notes)?;
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}

fn units<O: Os>(
    p: &mut Planner<'_, impl Exec>,
    os: &O,
    v: &Valid,
    start: Start,
    released: bool,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    // A role's units are started when the operator did not forbid it, a release is there to
    // run, and (for anything that serves the frontend) the operator asked for it by name.
    let mut hold = |why: &str| notes.push(why.to_owned());
    let start_roles = if start.no_start {
        hold("--no-start: units are enabled, not started");
        false
    } else if !released {
        hold("no release is installed yet: units are enabled, not started");
        false
    } else if v.role.serves_frontend() && !start.start_frontend {
        hold("the frontend is not started unasked: start it with --start-frontend or systemctl start");
        false
    } else {
        true
    };
    match os.platform() {
        Platform::MacOs => {
            if start_roles && !os.launchd_loaded(MAC_LABEL)? {
                p.run(
                    Cmd::new(hp(LAUNCHCTL)?)
                        .arg("bootstrap")
                        .arg("system")
                        .arg(format!("/Library/LaunchDaemons/{MAC_LABEL}.plist")),
                );
            }
        }
        Platform::Linux => {
            let installer_now = !start.no_start;
            let wanted = role_units(v.role)
                .iter()
                .map(|u| (*u, start_roles))
                .chain([(INSTALLER_UNIT, installer_now)]);
            for (unit, now) in wanted {
                if !os.unit_enabled(unit)? {
                    let mut cmd = Cmd::new(hp(SYSTEMCTL)?).arg("enable");
                    if now {
                        cmd = cmd.arg("--now");
                    }
                    p.run(cmd.arg("--").arg(unit));
                }
            }
        }
    }
    Ok(())
}
