//! The plan `setup` carries out, in two parts around the state database's schema: before it the
//! accounts, the directories, the tokens and `installer.toml`; after it `kbf.toml` and the
//! units. `kbf.toml` is the last file written and the units come after it, so a crash anywhere
//! before leaves no config that passes the units' `ConditionPathExists=`.
//!
//! Every step is added only where the host differs, so a second run with the same flags has
//! an empty plan.

use kbf_host::{Cmd, Entry, Exec, HostPath, Owner, Perms, Plan, Planner, RunUser};

use super::kbf_toml::{self, Decision, Ours};
use super::os::{Os, SYSTEMCTL, View};
use super::paths;
use super::validate::{Valid, mode_name};
use super::{SetupError, installer_settings};
use crate::cli::Mode;
use crate::layout::{Layout, Platform};

const USERADD: &str = "/usr/sbin/useradd";
/// The account a frontend and a storage node run as (`User=` of their units).
const FRONTEND_USER: &str = "kbf-server";
/// The unit of a frontend host.
const FRONTEND_UNIT: &str = "kbf-frontend.service";
/// The timer, and the drop-in of the service it started, that an earlier build's setup wrote
/// (a daily `pg_dump`): setup removes them from a host that has them.
const OLD_BACKUP_TIMER: &str = "kbf-backup.timer";
const OLD_BACKUP_DROPIN_DIR: &str = "/etc/systemd/system/kbf-backup.service.d";
const OLD_BACKUP_DROPIN: &str = "/etc/systemd/system/kbf-backup.service.d/10-service-user.conf";
/// The longest token file setup reads.
const MAX_TOKEN_BYTES: u64 = 4096;

/// The planner of this module: it reads the host through a [`View`].
pub(super) type Pl<'p, 'h> = Planner<'p, View<'h>>;

fn hp(path: &str) -> Result<HostPath, SetupError> {
    Ok(HostPath::new(path)?)
}

/// A directory at `path`, `mode` and `owner`'s. (One line per call site, so that a failed
/// step has a line of its own that a test runs.)
pub(super) fn dir(
    p: &mut Pl<'_, '_>,
    path: &str,
    mode: u32,
    owner: Owner,
) -> Result<(), SetupError> {
    p.dir(hp(path)?, Perms::new(mode), Some(owner))?;
    Ok(())
}

/// A file at `path` with these bytes, `mode` and `owner`'s; whether a step was planned.
pub(super) fn file(
    p: &mut Pl<'_, '_>,
    path: &str,
    mode: u32,
    owner: Owner,
    bytes: Vec<u8>,
) -> Result<bool, SetupError> {
    Ok(p.file(hp(path)?, Perms::new(mode), Some(owner), bytes)?)
}

/// The units a mode is run by. The names are the static unit files of the package
/// (`packaging/linux/units`); the installer's own service is separate, since it runs
/// whatever the mode. A frontend is run by `kbf-frontend.service`: the proxy's units are not a
/// mode's, and `setup` enables none of them.
pub fn mode_units(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Frontend => &[FRONTEND_UNIT],
        Mode::Storage => &["kbf-storage.service"],
    }
}

/// The user a mode's unit runs as (`User=` of the package's units), which is also the account
/// that logs in to the state database.
pub(crate) fn service_user_name(_mode: Mode) -> &'static str {
    FRONTEND_USER
}

/// Whether the host serves the operator API, which needs a token.
fn serves_api(mode: Mode) -> bool {
    mode == Mode::Frontend
}

pub fn fetch_user(platform: Platform) -> &'static str {
    let Platform::Linux = platform;
    "kbf-fetch"
}

/// What the plans share: who root and the service user are.
pub struct Ctx {
    pub platform: Platform,
    /// The installer's tree, for its paths.
    pub layout: Layout,
    pub root: Owner,
    /// The user the mode's unit runs as.
    pub service: RunUser,
}

/// The ids of `user` as the owner of a file.
fn owner_of(user: &RunUser) -> Owner {
    Owner {
        uid: user.uid,
        gid: user.gid,
    }
}

impl Ctx {
    /// The owner of what the service user reads or writes.
    fn service_owner(&self) -> Owner {
        owner_of(&self.service)
    }
}

/// The account `name`, or why the host cannot be set up: the package makes it.
fn account(os: &dyn Os, name: &'static str) -> Result<RunUser, SetupError> {
    os.user(name)?.ok_or(SetupError::NoUser { name })
}

pub fn context(os: &dyn Os, mode: Mode) -> Result<Ctx, SetupError> {
    let platform = os.platform();
    let service = account(os, service_user_name(mode))?;
    Ok(Ctx {
        platform,
        layout: Layout::new(kbf_host::Root::new("/")?, platform),
        root: os.root_owner(),
        service,
    })
}

/// The plan and what to tell the operator.
pub struct Outcome {
    pub plan: Plan,
    pub notes: Vec<String>,
}

/// A token file's content as setup keeps it: one line, a newline after it. The file is
/// looked at once, whole: what is checked is what is read.
fn read_token(exec: &dyn Exec, file: &str) -> Result<Vec<u8>, SetupError> {
    let bad = |why: &str| SetupError::Token(format!("{file}: {why}"));
    let (perms, bytes) = match exec.inspect(&hp(file)?)? {
        None => return Err(bad("no such file")),
        Some(Entry::File { perms, bytes, .. }) => (perms, bytes),
        Some(_) => return Err(bad("not a regular file")),
    };
    if perms.bits() & 0o077 != 0 {
        return Err(bad(&format!(
            "mode {perms} lets others read the token: chmod 600 {file}"
        )));
    }
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(bad("larger than 4 KiB, which no token is"));
    }
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

/// What an existing fetch account must be, for the confinement of the fetch child to mean
/// what the design says (`kbf-installer.md`: the child runs as `kbf-fetch`, with no fallback to
/// another user): a system account (not root, below the first
/// ordinary id), no login shell, no home. These are the values `setup` creates it with and
/// the package's sysusers entry makes.
struct FetchShape {
    below: u32,
    home: &'static str,
    shells: &'static [&'static str],
}

fn fetch_shape() -> FetchShape {
    FetchShape {
        below: 1000,
        home: "/nonexistent",
        shells: &["/usr/sbin/nologin", "/sbin/nologin"],
    }
}

/// Refuses an existing fetch account that is not the shape `setup` makes: the reason names
/// the account and the field. Nothing is changed on the host, since a wrong account may be
/// someone's, and the operator decides what to do with it.
fn check_fetch_shape(os: &dyn Os, user: &RunUser) -> Result<(), SetupError> {
    let want = fetch_shape();
    let shell = os.login_shell(&user.name)?;
    let mut wrong = Vec::new();
    if user.uid == 0 || user.uid >= want.below {
        wrong.push(format!(
            "uid {} (a system account: 1 to {})",
            user.uid,
            want.below - 1
        ));
    }
    if user.gid == 0 || user.gid >= want.below {
        wrong.push(format!(
            "gid {} (a system group: 1 to {})",
            user.gid,
            want.below - 1
        ));
    }
    if user.home != want.home {
        wrong.push(format!("home {:?} (want {:?})", user.home, want.home));
    }
    if !want.shells.contains(&shell.as_str()) {
        wrong.push(format!("shell {shell:?} (want {:?})", want.shells[0]));
    }
    if wrong.is_empty() {
        return Ok(());
    }
    Err(SetupError::NeedsOperator(format!(
        "the account {} exists and is not the fetch account setup makes: {}; the fetch child runs as it with no fallback to another user, so fix or remove the account, then run again",
        user.name,
        wrong.join(", ")
    )))
}

/// The fetch account is there with the right shape, or its creation is a step of `p`. One
/// function, so there is one definition of the account.
pub fn ensure_fetch_user(p: &mut Pl<'_, '_>, os: &dyn Os) -> Result<(), SetupError> {
    let name = fetch_user(os.platform());
    match os.user(name)? {
        Some(user) => check_fetch_shape(os, &user),
        None => create_fetch_user(p, name),
    }
}

fn create_fetch_user(p: &mut Pl<'_, '_>, name: &str) -> Result<(), SetupError> {
    p.run(
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
    );
    Ok(())
}

/// The part of the plan before the state database's schema: accounts, directories, the tokens
/// and `installer.toml`. Reads the host, changes nothing.
pub fn before_release(
    os: &dyn Os,
    exec: &dyn Exec,
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
    let settings = installer_settings(exec, v, platform)?;
    let mut notes = Vec::new();
    if settings.has_proxy_table() {
        notes.push(
            "installer.toml has a [proxy] table: the table is kept as it is, setup enables no proxy unit, and the unit of the mode is enabled as on a host without the proxy"
                .to_owned(),
        );
    }

    let view = View(exec);
    let mut p = Planner::new(&view);

    ensure_fetch_user(&mut p, os)?;

    // The installer's tree is root's; `orders/` inside it is the service user's.
    let (root, service) = (ctx.root, ctx.service_owner());
    dir(&mut p, paths::etc_dir(platform), 0o755, root)?;
    let state_dir = ctx.layout.state_dir().to_string();
    dir(&mut p, &state_dir, 0o755, root)?;
    dir(&mut p, ctx.layout.orders_dir().as_str(), 0o700, service)?;

    // The tokens belong to the user whose process reads them: both loaders refuse a token
    // file another user owns, or one with a mode other than 0600 or 0400.
    if let Some(token) = enroll_token {
        file(&mut p, paths::enroll_token(platform), 0o600, service, token)?;
    }
    if serves_api(v.mode) {
        operator_token(&mut p, os, exec, service, platform, &mut notes)?;
    }

    file(
        &mut p,
        paths::installer_toml(platform),
        0o644,
        root,
        settings.render().into_bytes(),
    )?;
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}

/// The first operator token: made once, when the file is not there. It is never printed; the
/// note names the file. Once a file is there, whatever it holds, setup makes no other.
fn operator_token(
    p: &mut Pl<'_, '_>,
    os: &dyn Os,
    exec: &dyn Exec,
    owner: Owner,
    platform: Platform,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    let token = paths::api_token(platform);
    if exec.stat(&hp(token)?)?.is_some() {
        return Ok(());
    }
    let secret = hex::encode(os.random(32)?);
    file(p, token, 0o600, owner, format!("{secret}\n").into_bytes())?;
    notes.push(format!(
        "the first operator token is in {token}: read it as root and keep it; setup makes no other"
    ));
    Ok(())
}

/// The text of the file at `path`; none when it is not there. A file that is not a regular
/// file is the operator's to deal with.
fn read_current(exec: &dyn Exec, path: &str) -> Result<Option<String>, SetupError> {
    let current = match exec.inspect(&hp(path)?)? {
        Some(Entry::File { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Some(_) => {
            return Err(SetupError::NeedsOperator(format!(
                "{path} is not a regular file"
            )));
        }
        None => None,
    };
    Ok(current)
}

/// Plans the file at `path` as `decision` says, mode 0640 and `owner`'s; `label` is its name in
/// the notes and `existed` whether it is there already.
fn config_file(
    p: &mut Pl<'_, '_>,
    label: &str,
    path: &str,
    existed: bool,
    decision: Decision,
    owner: Owner,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    match decision {
        Decision::Write(text) => {
            let changed = file(p, path, 0o640, owner, text.into_bytes())?;
            if changed && existed {
                notes.push(format!(
                    "{label} changed: a running kbf keeps the old one until it is restarted; setup does not restart it"
                ));
            }
        }
        Decision::Keep => notes.push(format!(
            "{label} holds other settings and the keys setup writes already match: left as it is"
        )),
        Decision::Refuse(why) => return Err(SetupError::NeedsOperator(why)),
    }
    Ok(())
}

/// The part after the state database's schema: `kbf.toml`, then the units. `released` says
/// whether there is a `kbf` to start: a release is installed already, or `/usr/bin/kbf` is
/// present. Reads the host, changes nothing.
pub fn after_release(
    os: &dyn Os,
    exec: &dyn Exec,
    v: &Valid,
    ctx: &Ctx,
    released: bool,
    no_start: bool,
) -> Result<Outcome, SetupError> {
    let view = View(exec);
    let mut p = Planner::new(&view);
    let mut notes = Vec::new();

    let platform = ctx.platform;
    let kbf_toml_path = paths::kbf_toml(platform);
    let ours = Ours {
        mode: mode_name(v.mode),
        frontend_url: v.frontend_url.as_deref(),
        token_file: v.token_file.as_ref().map(|_| paths::enroll_token(platform)),
        api_token_file: serves_api(v.mode).then_some(paths::api_token(platform)),
        state_store: v.database.as_deref(),
        extra: &v.kbf,
    };
    let current = read_current(exec, kbf_toml_path)?;
    let decision = kbf_toml::decide(current.as_deref(), &ours, kbf_toml_path);
    // root:<service group> 0640: the service reads it, nobody else; only root writes.
    let owner = Owner {
        uid: ctx.root.uid,
        gid: ctx.service_owner().gid,
    };
    let existed = current.is_some();
    config_file(
        &mut p,
        "kbf.toml",
        kbf_toml_path,
        existed,
        decision,
        owner,
        &mut notes,
    )?;

    notes.extend(units(&mut p, os, v, released, no_start)?);
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}

/// What an earlier build's setup left of its daily database backup: the timer it enabled and the
/// drop-in (and its directory) that gave the backup unit its user. Backups of Postgres are the
/// operator's, so this setup disables the timer and removes the drop-in. Plans nothing on a host
/// that never had them. Returns whether a unit definition changed, which needs a reload.
fn remove_old_backup_timer(
    p: &mut Pl<'_, '_>,
    os: &dyn Os,
    notes: &mut Vec<String>,
) -> Result<bool, SetupError> {
    let enabled = os.unit_enabled(OLD_BACKUP_TIMER)?;
    if enabled {
        p.run(
            Cmd::new(hp(SYSTEMCTL)?)
                .arg("disable")
                .arg("--now")
                .arg("--")
                .arg(OLD_BACKUP_TIMER),
        );
    }
    // The file first: the directory is removed only when empty.
    let dropin = p.absent(hp(OLD_BACKUP_DROPIN)?)?;
    let dir = p.absent(hp(OLD_BACKUP_DROPIN_DIR)?)?;
    if enabled || dropin || dir {
        notes.push(format!(
            "{OLD_BACKUP_TIMER} (the database backup an earlier setup enabled) is disabled and its drop-in removed: kbf no longer backs up Postgres, which is the operator's"
        ));
    }
    Ok(dropin || dir)
}

/// What `setup` tells of the unit it enables and leaves stopped on a host that keeps its state
/// in Postgres: starting one takes the database, the TLS files and the ports, which only the
/// operator decides.
fn not_started_note() -> &'static str {
    "the frontend unit is enabled, not started: start it when the database, the TLS files and the port are ready"
}

fn units(
    p: &mut Pl<'_, '_>,
    os: &dyn Os,
    v: &Valid,
    released: bool,
    no_start: bool,
) -> Result<Vec<String>, SetupError> {
    let mut notes = Vec::new();
    // The mode's units are started when the operator did not forbid it and there is a `kbf`
    // to start (a release is installed, or `/usr/bin/kbf` is present). The installer's own service has
    // nothing to wait for. A unit that makes a frontend is never started here: starting one takes the state database and the public
    // port, which only the operator decides.
    let start_mode = if v.mode.keeps_state_in_database() {
        notes.push(not_started_note().to_owned());
        false
    } else if no_start {
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
    let backup_gone = remove_old_backup_timer(p, os, &mut notes)?;
    // Only the unit of the mode: the `kbf` package ships no other unit (the installer's
    // and the proxy's are not in it).
    let wanted = mode_units(v.mode).iter().map(|u| (*u, start_mode));
    if backup_gone {
        p.run(Cmd::new(hp(SYSTEMCTL)?).arg("daemon-reload"));
    }
    for (unit, now) in wanted {
        if !os.unit_enabled(unit)? {
            let mut cmd = Cmd::new(hp(SYSTEMCTL)?).arg("enable");
            if now {
                cmd = cmd.arg("--now");
            }
            p.run(cmd.arg("--").arg(unit));
        }
    }
    Ok(notes)
}
