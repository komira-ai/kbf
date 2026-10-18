//! The plan `setup` carries out, in two parts around the first release: before it the accounts,
//! the directories, the tokens and `installer.toml` (the installer needs its settings to
//! fetch); after it `kbf.toml` and the units. `kbf.toml` is the last file written and the
//! units come after it, so a crash anywhere before leaves no config that passes the units'
//! `ConditionPathExists=`.
//!
//! Every step is added only where the host differs, so a second run with the same flags has
//! an empty plan.

use kbf_host::{Cmd, Entry, Exec, HostPath, Kind, Owner, Perms, Plan, Planner, RunUser};

use super::kbf_toml::{self, Decision, Ours};
use super::os::{DSCL, LAUNCHCTL, Os, SYSTEMCTL};
use super::paths;
use super::validate::{Valid, mode_name};
use super::{SetupError, installer_file};
use crate::cli::Mode;
use crate::layout::Platform;

const USERADD: &str = "/usr/sbin/useradd";
const USERMOD: &str = "/usr/sbin/usermod";
const LOGINCTL: &str = "/usr/bin/loginctl";
/// The subid range a new grant starts at, and its size (what `usermod` documents).
const SUBID_FLOOR: u64 = 100_000;
const SUBID_COUNT: u64 = 65_536;
/// The ids setup considers for the macOS fetch account (below 500 is a system account).
const MAC_ID_FROM: u32 = 300;
const MAC_ID_TO: u32 = 400;
/// The launchd labels: the installer's plist is the package's; the daemon's is the release's.
pub const MAC_INSTALLER_LABEL: &str = "com.komira.kbf-installer";
pub const MAC_DAEMON_LABEL: &str = "com.komira.kbf-daemon";
pub const INSTALLER_UNIT: &str = "kbf-installer.service";
/// The longest token file setup reads.
const MAX_TOKEN_BYTES: u64 = 4096;

fn hp(path: &str) -> Result<HostPath, SetupError> {
    Ok(HostPath::new(path)?)
}

/// The units a mode is run by. The names are the static unit files of the package
/// (`packaging/linux/units`); the installer's own service is separate, since it runs
/// whatever the mode.
pub fn mode_units(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Frontend => &["kbf-envoy.service", "kbf-frontend@a.service"],
        Mode::Storage => &["kbf-storage.service"],
        Mode::Daemon => &["kbf-daemon.service"],
        Mode::StorageAndDaemon => &["kbf-storage-and-daemon.service"],
        Mode::AllInOne => &["kbf-envoy.service", "kbf-all-in-one.service"],
    }
}

/// The user a mode's unit runs as on Linux (`User=` of the package's units).
fn service_user_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Frontend | Mode::Storage => "kbf-server",
        Mode::Daemon | Mode::StorageAndDaemon | Mode::AllInOne => "kbf",
    }
}

/// Whether the host runs actions in containers: it then needs a subid range and lingering.
fn runs_actions(mode: Mode) -> bool {
    matches!(mode, Mode::Daemon | Mode::StorageAndDaemon | Mode::AllInOne)
}

/// Whether the host serves the operator API, which needs a token.
fn serves_api(mode: Mode) -> bool {
    matches!(mode, Mode::Frontend | Mode::AllInOne)
}

fn fetch_user(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "kbf-fetch",
        Platform::MacOs => "_kbf-fetch",
    }
}

/// What the plans share: who root and the service user are.
pub struct Ctx {
    pub platform: Platform,
    pub root: Owner,
    /// The user the mode's unit runs as (Linux); a Mac runs the daemon as root.
    pub service: Option<RunUser>,
}

impl Ctx {
    /// The owner of what the service user reads or writes; root when there is none.
    fn service_owner(&self) -> Owner {
        self.service.as_ref().map_or(self.root, |u| Owner {
            uid: u.uid,
            gid: u.gid,
        })
    }
}

pub fn context<O: Os>(os: &O, mode: Mode) -> Result<Ctx, SetupError> {
    let platform = os.platform();
    if platform == Platform::MacOs && mode != Mode::Daemon {
        return Err(SetupError::Unsupported(format!(
            "--mode={}: a Mac is a node only (--mode=daemon)",
            mode_name(mode)
        )));
    }
    let service = match platform {
        Platform::Linux => {
            let name = service_user_name(mode);
            Some(os.user(name)?.ok_or(SetupError::NoUser { name })?)
        }
        Platform::MacOs => None,
    };
    Ok(Ctx {
        platform,
        root: os.root_owner(),
        service,
    })
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
    let meta = exec.stat(&path)?.ok_or_else(|| bad("no such file"))?;
    if meta.kind != Kind::File {
        return Err(bad("not a regular file"));
    }
    if meta.perms.bits() & 0o077 != 0 {
        return Err(bad(&format!(
            "mode {} lets others read the token: chmod 600 {file}",
            meta.perms
        )));
    }
    if meta.len > MAX_TOKEN_BYTES {
        return Err(bad("larger than 4 KiB, which no token is"));
    }
    let Some(Entry::File { bytes, .. }) = exec.inspect(&path)? else {
        return Err(bad("not a regular file"));
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| bad("not text"))?
        .trim();
    if text.is_empty() {
        return Err(bad("empty"));
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("holds more than one word: a token is a single line"));
    }
    Ok(format!("{text}\n").into_bytes())
}

/// `/etc/subuid`-style text has a range for `user` (keyed by name or uid).
fn has_range(text: &str, user: &RunUser) -> bool {
    text.lines().any(|line| {
        let mut f = line.split(':');
        let key = f.next();
        let count = f.nth(1).and_then(|c| c.parse::<u64>().ok());
        (key == Some(user.name.as_str()) || key == Some(user.uid.to_string().as_str()))
            && count.is_some_and(|c| c > 0)
    })
}

/// Where a new range may start: above every range in `text`, so it overlaps no other user's.
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

/// The part of the plan before the first release: accounts, directories, the tokens and
/// `installer.toml`. Reads the host, changes nothing.
pub fn before_release<O: Os, E: Exec>(
    os: &O,
    exec: &E,
    v: &Valid,
    ctx: &Ctx,
) -> Result<Outcome, SetupError> {
    let platform = ctx.platform;
    // Read what can be wrong first, so a bad token or a bad settings file stops the run
    // before any step is planned.
    let enroll_token = v
        .token_file
        .as_deref()
        .map(|file| read_token(exec, file))
        .transpose()?;
    let settings = installer_file(exec, v, platform)?;

    let mut p = Planner::new(exec);
    let mut notes = Vec::new();

    let fetch = fetch_user(platform);
    if os.user(fetch)?.is_none() {
        create_fetch_user(&mut p, os, fetch)?;
    }
    if let (Some(user), true) = (&ctx.service, runs_actions(v.mode)) {
        add_subids(&mut p, user, "subuid", &os.subuid()?)?;
        add_subids(&mut p, user, "subgid", &os.subgid()?)?;
        if !os.lingers(&user.name)? {
            p.run(Cmd::new(hp(LOGINCTL)?).arg("enable-linger").arg(&user.name));
        }
    }

    // The installer's tree is root's; `orders/` inside it is the service user's (on a Mac the
    // daemon is root, so it is root's).
    let (root, service) = (ctx.root, ctx.service_owner());
    p.dir(hp(paths::ETC_DIR)?, Perms::new(0o755), Some(root))?;
    p.dir(
        hp(paths::installer_dir(platform))?,
        Perms::new(0o755),
        Some(root),
    )?;
    p.dir(
        hp(&paths::orders_dir(platform))?,
        Perms::new(0o700),
        Some(service),
    )?;

    // The tokens belong to the user whose process reads them: both loaders refuse a token
    // file another user owns, or one with a mode other than 0600 or 0400.
    if let Some(token) = enroll_token {
        p.file(
            hp(paths::ENROLL_TOKEN)?,
            Perms::new(0o600),
            Some(service),
            token,
        )?;
    }
    if serves_api(v.mode) {
        operator_token(&mut p, os, exec, service, &mut notes)?;
    }

    p.file(
        hp(paths::INSTALLER_TOML)?,
        Perms::new(0o644),
        Some(root),
        settings.into_bytes(),
    )?;
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}

/// The first operator token: made once, when the file is not there. It is never printed; the
/// note names the file. Once a file is there, whatever it holds, setup makes no other.
fn operator_token<O: Os, E: Exec>(
    p: &mut Planner<'_, E>,
    os: &O,
    exec: &E,
    owner: Owner,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    let path = hp(paths::API_TOKEN)?;
    if exec.stat(&path)?.is_some() {
        return Ok(());
    }
    let secret = hex::encode(os.random(32)?);
    p.file(path, Perms::new(0o600), Some(owner), format!("{secret}\n").into_bytes())?;
    notes.push(format!(
        "the first operator token is in {}: read it as root and keep it; setup makes no other",
        paths::API_TOKEN
    ));
    Ok(())
}

/// The part after the first release: `kbf.toml`, then the units. `released` says whether a
/// release is installed (or, in a dry run, would be). Reads the host, changes nothing.
pub fn after_release<O: Os, E: Exec>(
    os: &O,
    exec: &E,
    v: &Valid,
    ctx: &Ctx,
    released: bool,
    no_start: bool,
) -> Result<Outcome, SetupError> {
    let mut p = Planner::new(exec);
    let mut notes = Vec::new();

    let ours = Ours {
        mode: mode_name(v.mode),
        frontend_url: v.frontend_url.as_deref(),
        token_file: v.token_file.as_ref().map(|_| paths::ENROLL_TOKEN),
        api_token_file: serves_api(v.mode).then_some(paths::API_TOKEN),
    };
    let current = match exec.inspect(&hp(paths::KBF_TOML)?)? {
        Some(Entry::File { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Some(_) => {
            return Err(SetupError::NeedsOperator(format!(
                "{} is not a regular file",
                paths::KBF_TOML
            )));
        }
        None => None,
    };
    match kbf_toml::decide(current.as_deref(), &ours) {
        Decision::Write(text) => {
            let existed = current.is_some();
            // root:<service group> 0640: the service reads it, nobody else; only root writes.
            let owner = Owner {
                uid: ctx.root.uid,
                gid: ctx.service_owner().gid,
            };
            let changed = p.file(
                hp(paths::KBF_TOML)?,
                Perms::new(0o640),
                Some(owner),
                text.into_bytes(),
            )?;
            if changed && existed {
                notes.push(
                    "kbf.toml changed: a running kbf keeps the old one until it is restarted; setup does not restart it"
                        .to_owned(),
                );
            }
        }
        Decision::Keep { other_settings } => {
            if other_settings {
                notes.push(
                    "kbf.toml holds other settings and the keys setup writes already match: left as it is"
                        .to_owned(),
                );
            }
        }
        Decision::Refuse(why) => return Err(SetupError::NeedsOperator(why)),
    }

    units(&mut p, os, v, ctx.platform, released, no_start, &mut notes)?;
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}

fn units<O: Os>(
    p: &mut Planner<'_, impl Exec>,
    os: &O,
    v: &Valid,
    platform: Platform,
    released: bool,
    no_start: bool,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    // The mode's units are started when the operator did not forbid it and a release is
    // there to run. The installer's own service has nothing to wait for.
    let start_mode = if no_start {
        notes.push("--no-start: units are enabled, not started".to_owned());
        false
    } else if !released {
        notes.push(
            "no release is installed yet: the units are enabled, not started; the first update installs one"
                .to_owned(),
        );
        false
    } else {
        true
    };
    match platform {
        Platform::MacOs => {
            let wanted = [
                (MAC_INSTALLER_LABEL, !no_start),
                (MAC_DAEMON_LABEL, start_mode),
            ];
            for (label, now) in wanted {
                if now && !os.launchd_loaded(label)? {
                    p.run(
                        Cmd::new(hp(LAUNCHCTL)?)
                            .arg("bootstrap")
                            .arg("system")
                            .arg(format!("/Library/LaunchDaemons/{label}.plist")),
                    );
                }
            }
        }
        Platform::Linux => {
            let wanted = mode_units(v.mode)
                .iter()
                .map(|u| (*u, start_mode))
                .chain([(INSTALLER_UNIT, !no_start)]);
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
