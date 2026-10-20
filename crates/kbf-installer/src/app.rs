//! What the binary does with a parsed command line: the verbs that exist run, the ones a later
//! pull request builds say so (`not implemented (P<n>)`, exit 1) after the checks every verb
//! shares (root, `--json`, `--apply`).
//!
//! `run`, `boot-check` and the operator's verbs (`confirm`, `revert`, `rollback`, `gc`,
//! `quiesce`) keep their own checks: they work on a scratch root as an ordinary user.

use std::ffi::CString;
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_host::{HostExec, HostPath, Root};

use crate::activate;
use crate::backup;
use crate::cli::{
    BackupArgs, BootArgs, Cli, MigrateArgs, ReleaseSource, RestoreArgs, RunArgs, ServiceArgs, Verb,
};
use crate::database::Database;
use crate::exit::Exit;
use crate::helper::cycle::{Config, Helper, HelperError};
use crate::helper::fetcher_host::HostFetcher;
use crate::helper::host_mode;
use crate::helper::leases;
use crate::helper::operator::{self, Operator};
use crate::helper::restart::Restart;
use crate::helper::serve::{FileIdentity, Flow, PASS_INTERVAL, Serve};
use crate::helper::service::HostProcs;
use crate::layout::{Layout, Platform};
use crate::migrate;
use crate::proxy_verb;
use crate::release::ReleaseError;
use crate::setup;

/// Why a verb failed and the exit it ends with: `Failed`, or `NeedsOperator` when only an
/// operator can go on (the state files are both unusable).
#[derive(Debug)]
struct Failure {
    exit: Exit,
    why: String,
}

impl From<String> for Failure {
    fn from(why: String) -> Failure {
        Failure {
            exit: Exit::Failed,
            why,
        }
    }
}

impl From<operator::Failure> for Failure {
    fn from(f: operator::Failure) -> Failure {
        let exit = match &f {
            operator::Failure::Failed(_) => Exit::Failed,
            operator::Failure::Refused(_) => Exit::Refused,
            operator::Failure::Busy(_) => Exit::Busy,
            operator::Failure::NeedsOperator(_) => Exit::NeedsOperator,
        };
        Failure {
            exit,
            why: f.to_string(),
        }
    }
}

impl From<HelperError> for Failure {
    fn from(e: HelperError) -> Failure {
        let operator = matches!(&e, HelperError::State(s) if s.needs_operator());
        Failure {
            exit: if operator {
                Exit::NeedsOperator
            } else {
                Exit::Failed
            },
            why: e.to_string(),
        }
    }
}

impl From<ReleaseError> for Failure {
    fn from(e: ReleaseError) -> Failure {
        let operator = matches!(&e, ReleaseError::State(s) if s.needs_operator());
        Failure {
            exit: if operator {
                Exit::NeedsOperator
            } else {
                Exit::Failed
            },
            why: e.to_string(),
        }
    }
}

/// A release error keeps the exit of the release verbs; files that are not a release are a
/// refusal (exit 2), not a failure to try again.
impl From<crate::record_release::RecordError> for Failure {
    fn from(e: crate::record_release::RecordError) -> Failure {
        match e {
            crate::record_release::RecordError::Release(e) => Failure::from(e),
            other => Failure {
                exit: Exit::Refused,
                why: other.to_string(),
            },
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host(root: PathBuf) -> Result<(Layout, HostExec), String> {
    let root = Root::new(root).map_err(|e| e.to_string())?;
    Ok((
        Layout::new(root.clone(), Platform::current()),
        HostExec::new(root),
    ))
}

/// `path` (an absolute path on the host) under the host root.
fn under(root: &Path, path: &HostPath) -> Result<PathBuf, Failure> {
    let root = Root::new(root).map_err(|e| Failure::from(e.to_string()))?;
    Ok(root.join(path))
}

/// What `run` restarts after a swap, and the `installer.toml` it migrates the state database
/// from.
struct Target {
    service: ServiceArgs,
    installer_config: Option<HostPath>,
}

/// The target of `run`: the service its flags name, else (Linux) the unit of the mode in the
/// host's `kbf.toml`, which is how the one packaged `kbf-installer.service` serves every mode.
/// A unit found that way on a frontend or all-in-one host reads the host's `installer.toml`
/// (`/etc/kbf`) when `--installer-config` is not given (`setup` wrote it there). A Mac derives
/// nothing: it is a daemon-only node whose property list always names its launchd label, so
/// `run` there needs `--launchd-label`.
fn target_of(root: &Path, args: &RunArgs) -> Result<Target, Failure> {
    target_on(root, args, Platform::current())
}

/// [`target_of`] for the host of `platform`, which the layout (the `kbf.toml` it reads, the
/// `installer.toml` it defaults to) is of: the platform is a parameter so that a test can say
/// which platform's rule it pins.
fn target_on(root: &Path, args: &RunArgs, platform: Platform) -> Result<Target, Failure> {
    if let Some(service) = args.named_service() {
        return Ok(Target {
            service,
            installer_config: args.installer_config.clone(),
        });
    }
    let root = Root::new(root.to_path_buf()).map_err(|e| usage(e.to_string()))?;
    let layout = Layout::new(root, platform);
    let service = derived_service(&layout, platform)?;
    let default = (args.installer_config.is_none() && service.runs_database_mode())
        .then(|| layout.installer_toml());
    let installer_config = args.installer_config.clone().or(default);
    Ok(Target {
        service,
        installer_config,
    })
}

fn usage(why: String) -> Failure {
    Failure {
        exit: Exit::Usage,
        why,
    }
}

/// The service of `layout`'s host when no flag names one.
fn derived_service(layout: &Layout, platform: Platform) -> Result<ServiceArgs, Failure> {
    if platform != Platform::Linux {
        return Err(usage("run needs --launchd-label".to_owned()));
    }
    let unit = host_mode::mode_unit(layout).map_err(|why| {
        usage(format!(
            "run needs --unit (or run setup, which writes the kbf.toml that names the mode): {why}"
        ))
    })?;
    Ok(ServiceArgs {
        unit: Some(unit.to_owned()),
        launchd_label: None,
    })
}

fn run(root: PathBuf, args: &RunArgs) -> Result<(), Failure> {
    // A running program has a file, and the file can be read: neither can be told otherwise.
    let me = std::env::current_exe().expect("a running program has a file");
    let watch = FileIdentity::of(&me).expect("the file I run from can be read");
    run_from(root, args, &me, watch)
}

/// The `installer.toml` the fetcher reads: `--installer-config`, else the file `setup` writes
/// on the host's platform.
fn installer_toml_of(args: &RunArgs, layout: &Layout) -> HostPath {
    match &args.installer_config {
        Some(path) => path.clone(),
        None => layout.installer_toml(),
    }
}

/// The helper `run` drives: the host's, with the schema step of `--installer-config` attached
/// when the flag is given.
fn build_helper(root: PathBuf, args: &RunArgs) -> Result<Helper<HostExec>, Failure> {
    let target = target_of(&root, args)?;
    let database = match &target.installer_config {
        Some(config) => Some(
            Database::from_installer_toml(&under(&root, config)?)
                .map_err(|e| Failure::from(e.to_string()))?,
        ),
        None => None,
    };
    let (layout, exec) = host(root)?;
    let platform = layout.platform();
    let cfg = Config {
        update_dir: args.update_dir_in(&layout),
        orders_dir: args.orders_dir_in(&layout),
        restart: target.service.restart(),
        window_secs: args.window_secs,
        starts: args.starts,
        min_activation_interval_secs: args.min_activation_interval_secs,
    };
    // The leases are those of the unit restarted after a swap, read below the host root's
    // cgroup mount; the helper and each fetch's pipeline get a source of their own. The
    // fetcher reads `installer.toml` (the flag's, else the host's own) at each fetch.
    let cgroups = HostPath::new(leases::CGROUP_ROOT).expect("CGROUP_ROOT is an absolute path");
    let cgroup_root = layout.join(&cgroups);
    let lease_source = leases::for_restart(&cfg.restart, &cgroup_root);
    let restart = cfg.restart.clone();
    let installer_toml = installer_toml_of(args, &layout);
    let fetcher = HostFetcher::on_host(layout.clone(), installer_toml, move || {
        leases::for_restart(&restart, &cgroup_root)
    });
    let mut helper = Helper::new(
        layout,
        exec,
        cfg,
        Box::new(fetcher),
        lease_source,
        Box::new(HostProcs::new(platform)),
    );
    if let Some(database) = database {
        helper.set_schema_step(Box::new(database));
    }
    Ok(helper)
}

/// The service loop of `me`, the file this program runs from, whose identity `watch` holds:
/// once the file is replaced the loop starts the new one in this process's place.
fn run_from(root: PathBuf, args: &RunArgs, me: &Path, watch: FileIdentity) -> Result<(), Failure> {
    // A frontend or all-in-one unit activates a release only after the database is migrated
    // for it; without the installer.toml that names the database the step would be silently
    // absent, so the service does not start.
    let target = target_of(&root, args)?;
    if target.installer_config.is_none() && target.service.runs_database_mode() {
        return Err(Failure {
            exit: Exit::Usage,
            why: "run needs --installer-config on a frontend or all-in-one host (the \
                  database is migrated before a release is activated)"
                .to_owned(),
        });
    }
    let helper = build_helper(root, args)?;
    let mut serve = Serve::new(helper, Box::new(watch));
    serve.helper().recover()?;
    let mut left = args.passes;
    while left != Some(0) {
        match serve.turn(now())? {
            Flow::Continue => {}
            Flow::Reexec => {
                eprintln!("kbf-installer: my file was replaced; starting the new one");
                let err = reexec(me);
                return Err(format!("cannot start {}: {err}", me.display()).into());
            }
        }
        serve.helper().drain_log(&mut std::io::stderr());
        left = left.map(|n| n - 1);
        if left != Some(0) {
            sleep(PASS_INTERVAL);
        }
    }
    Ok(())
}

fn boot_check(root: PathBuf, args: &BootArgs) -> Result<(), Failure> {
    let (layout, mut exec) = host(root)?;
    let mut out = Vec::new();
    let result = activate::boot_check(&mut exec, &layout, now(), args.window_secs, &mut out);
    eprint!("{}", String::from_utf8_lossy(&out));
    match result {
        Ok(decision) => {
            eprintln!("kbf-installer: boot check: {decision:?}");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// The user that must own every file the migration reads and writes. On a host every file
/// belongs to root. Under `--root` (a rehearsal, a test) the tree is the caller's own, so the
/// owner to expect is the caller: nobody gains anything by it, since root and a stranger
/// alike only reach what they already could.
pub(crate) fn owner_uid(rehearsal: bool, euid: u32) -> u32 {
    if rehearsal { euid } else { 0 }
}

/// The user the version probe runs as: the failure when the host cannot say, has no such
/// user, or the user is root (outside a rehearsal). (The generic part is one call; what it
/// does with the answer is [`user_found`], so that there is one copy of it to run.)
fn probe_user<E: kbf_host::Exec>(
    exec: &E,
    name: &str,
    rehearsal: bool,
    platform: Platform,
) -> Result<kbf_host::RunUser, Failure> {
    let user = user_found(
        kbf_host::user::lookup_on(exec, name, platform.user_db()),
        name,
    )?;
    // The probe runs a binary nobody has trusted yet, and who it runs as is the whole of what
    // it is confined by (settings refuse root for the same reason). A rehearsal runs on the
    // caller's own tree as the caller, whoever that is.
    if !rehearsal && (user.uid == 0 || user.gid == 0) {
        return Err(Failure {
            exit: Exit::Usage,
            why: format!(
                "--fetch-user {name:?} is root (uid {}, gid {}): the version probe runs \
                 binaries nobody has trusted yet and must not run as root",
                user.uid, user.gid
            ),
        });
    }
    Ok(user)
}

fn user_found(
    found: Result<Option<kbf_host::RunUser>, kbf_host::user::UserError>,
    name: &str,
) -> Result<kbf_host::RunUser, Failure> {
    found
        .map_err(|e| Failure::from(e.to_string()))?
        .ok_or_else(|| Failure {
            exit: Exit::Usage,
            why: format!("no user {name:?} to run the version probe as"),
        })
}

/// `migrate`: the report to print, or why it stopped.
fn migrate_verb(root: PathBuf, apply: bool, args: &MigrateArgs) -> Result<String, Failure> {
    let rehearsal = root != Path::new("/");
    let (layout, mut exec) = host(root)?;
    let owner = owner_uid(rehearsal, rustix::process::geteuid().as_raw());
    let (user, made) = probe_account(&mut exec, &layout, args, apply, rehearsal, owner)?;
    // A node that is not on a host with the user simply has no owner for its spool: the
    // migration says so and leaves the node's `kbf.toml` as it is.
    let node_user = kbf_host::user::lookup_on(
        &exec,
        crate::setup::plan::service_user_name(crate::cli::Mode::Daemon),
        layout.platform().user_db(),
    )
    .ok()
    .flatten();
    let m = migrate::Migration {
        roots: migrate::Roots::new(layout.platform()),
        layout,
        run_as: crate::run::RunAs {
            uid: user.uid,
            gid: user.gid,
        },
        owner_uid: owner,
        units: args.units.clone(),
        helper_unit: args.helper_unit.clone(),
        installer_unit: args.installer_unit.clone(),
        seed: migrate::Seed {
            source: match args.release_source {
                ReleaseSource::Url => migrate::Source::Url,
                ReleaseSource::Frontend => migrate::Source::Frontend,
            },
            min_generation: args.min_generation,
        },
        node_user,
        lock_wait: operator::LOCK_WAIT,
    };
    let mode = if apply {
        kbf_host::Mode::Apply
    } else {
        kbf_host::Mode::DryRun
    };
    let report = if args.rollback {
        migrate::rollback(&m, &mut exec, mode)
    } else {
        // Nothing can fetch a release from its commit alone yet, and there is no frontend
        // client: every release is adopted unverified, and the report says so.
        let hooks = migrate::Hooks {
            runner: &crate::run::SystemRunner,
            verifier: &migrate::NotWired,
            frontend: &migrate::NoFrontend,
        };
        migrate::migrate(&m, &mut exec, &hooks, mode)
    }
    .map_err(|e| Failure {
        exit: e.exit(),
        why: e.to_string(),
    })?;
    let mut shown = made;
    shown.push(report.to_string().trim_end().to_owned());
    Ok(shown.join("\n"))
}

/// The account the version probe runs as, and the lines of the steps that made it. The first
/// step of an applying migration (before the first look at the old layout runs a binary): on a
/// Mac with the platform's own fetch account (no `--fetch-user`, or its name), that has an old
/// layout to migrate, the account is made when it is not there ([`make_fetch_user`]). Not in a
/// dry run (it changes nothing, so the account must exist already, and says how it comes to),
/// not in a rollback, and not in a rehearsal under `--root` (the host's accounts are not the
/// rehearsal's to change).
fn probe_account<E: kbf_host::Exec>(
    exec: &mut E,
    layout: &Layout,
    args: &MigrateArgs,
    apply: bool,
    rehearsal: bool,
    owner: u32,
) -> Result<(kbf_host::RunUser, Vec<String>), Failure> {
    let platform = layout.platform();
    let standard = setup::plan::fetch_user(platform);
    let name = args.fetch_user.as_deref().unwrap_or(standard);
    let mut made = Vec::new();
    if apply
        && !args.rollback
        && !rehearsal
        && name == standard
        && matches!(
            migrate::guard(layout, owner),
            Err(migrate::MigrateError::Unmigrated(_))
        )
    {
        made = make_fetch_user(exec, platform)?;
    }
    let user = probe_user(&*exec, name, rehearsal, platform).map_err(|f| {
        if !apply && name == standard && platform == Platform::MacOs {
            Failure {
                why: format!(
                    "{}; a dry run changes nothing and cannot make it: `migrate --apply` creates it, the way setup does",
                    f.why
                ),
                ..f
            }
        } else {
            f
        }
    })?;
    Ok((user, made))
}

/// The fetch account on a Mac, made by the plan `setup` uses for it ([`setup::plan::fetch_user_plan`]:
/// one definition of the account, so what `migrate` makes is what `setup` finds): the lines
/// of the steps run, none when the account is there with the shape `setup` makes. An account
/// of the wrong shape (uid, shell, home) is refused, not changed. On Linux the package's
/// sysusers entry makes `kbf-fetch` when the package is installed, which `migrate` needs
/// anyway, so there is nothing to make (and `setup` is not a prerequisite there either).
fn make_fetch_user<E: kbf_host::Exec>(
    exec: &mut E,
    platform: Platform,
) -> Result<Vec<String>, Failure> {
    if platform != Platform::MacOs {
        return Ok(Vec::new());
    }
    let plan = {
        let os = setup::os::SystemOs::new(&*exec, platform);
        setup::plan::fetch_user_plan(&os, &*exec)
    }
    .map_err(|e| Failure {
        exit: e.exit(),
        why: e.to_string(),
    })?;
    let mut lines = Vec::new();
    for step in plan.steps() {
        exec.apply(step)
            .map_err(|e| Failure::from(format!("{step}: {e}")))?;
        lines.push(format!("+ {step}"));
    }
    Ok(lines)
}

/// Refuses while the old layout's state is the host's and no migration finished.
fn old_layout_guard(root: &Path) -> Result<(), Failure> {
    let rehearsal = root != Path::new("/");
    let (layout, _) = host(root.to_owned())?;
    let owner = owner_uid(rehearsal, rustix::process::geteuid().as_raw());
    migrate::guard(&layout, owner).map_err(|e| Failure {
        exit: e.exit(),
        why: e.to_string(),
    })
}

/// A verb of the operator, run inline under the lock.
fn operate(
    root: PathBuf,
    apply: bool,
    restart: Restart,
    verb: impl FnOnce(&mut Operator<'_, HostExec>) -> Result<(), operator::Failure>,
) -> Result<(), Failure> {
    let (layout, mut exec) = host(root)?;
    let procs = HostProcs::new(layout.platform());
    let mut err = std::io::stderr();
    verb(&mut Operator {
        host: &layout,
        exec: &mut exec,
        restart: &restart,
        procs: &procs,
        out: &mut err,
        apply,
        wait: operator::LOCK_WAIT,
    })
    .map_err(Failure::from)
}

fn record_release(
    root: PathBuf,
    euid: u32,
    apply: bool,
    commit: &crate::CommitId,
) -> Result<(), Failure> {
    let (layout, mut exec) = host(root)?;
    crate::record_release::record(
        &mut exec,
        &layout,
        euid,
        commit,
        apply,
        &mut std::io::stderr(),
    )
    .map_err(Failure::from)
}

fn gc(root: PathBuf, apply: bool, keep: usize) -> Result<(), Failure> {
    let (layout, mut exec) = host(root)?;
    let procs = HostProcs::new(layout.platform());
    operator::gc(
        &mut exec,
        &layout,
        &procs,
        keep,
        apply,
        &mut std::io::stderr(),
    )
    .map_err(Failure::from)
}

fn quiesce(root: PathBuf) -> Result<(), Failure> {
    let (layout, _) = host(root)?;
    operator::quiesce(&layout, operator::LOCK_WAIT).map_err(Failure::from)
}

/// What `backup` and `restore` say when they work, on the error stream (standard output is
/// for `--json`).
fn progress(err: &mut dyn Write, text: &str) {
    // A closed pipe is the reader's business; the exit code still tells.
    let _ = writeln!(err, "kbf-installer: {text}");
}

/// `kbf-installer backup`: without `--apply` it says what it would do and touches nothing.
fn run_backup(
    tool: &dyn backup::Tool,
    root: PathBuf,
    apply: bool,
    args: &BackupArgs,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let settings = backup::Settings::read(&under(&root, &args.installer_config)?)?;
    let dir = under(&root, &args.dir)?;
    if !apply {
        progress(
            err,
            &format!(
                "backup: dry run: would dump the state database into {}, keeping the newest {}",
                dir.display(),
                settings.keep
            ),
        );
        return Ok(());
    }
    let spec = backup::Spec {
        dir: &dir,
        url: &settings.url,
        keep: settings.keep,
        pg_dump: &args.pg_dump,
        pg_restore: &args.pg_restore,
    };
    let done = backup::backup(tool, &spec, now())?;
    progress(
        err,
        &format!(
            "backup: wrote {} ({} bytes); removed {} old",
            done.file.display(),
            done.bytes,
            done.removed.len()
        ),
    );
    Ok(())
}

/// `kbf-installer restore`: without `--apply` it says what it would do and touches nothing.
fn run_restore(
    tool: &dyn backup::Tool,
    root: PathBuf,
    apply: bool,
    args: &RestoreArgs,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let url = match &args.database {
        // A bare name: a scratch database on the configured server, for the configured role.
        Some(name) if !name.contains(['/', ':']) => {
            let settings = backup::Settings::read(&under(&root, &args.installer_config)?)?;
            settings.target.url_for(name)?
        }
        Some(url) => {
            crate::database::check_url("the --database flag", url)
                .map_err(|e| Failure::from(e.to_string()))?;
            url.clone()
        }
        None => backup::Settings::read(&under(&root, &args.installer_config)?)?.url,
    };
    let file = under(&root, &args.file)?;
    if !apply {
        progress(
            err,
            &format!(
                "restore: dry run: would restore {} into the state database",
                file.display()
            ),
        );
        return Ok(());
    }
    backup::restore(tool, &args.pg_restore, &url, &file)?;
    progress(err, &format!("restore: restored {}", file.display()));
    Ok(())
}

/// The directory the host's paths are under: `/`, except in a build with the sandbox flag.
fn root_of(cli: &Cli) -> PathBuf {
    #[cfg(feature = "test-sandbox")]
    return cli.root.clone();
    #[cfg(not(feature = "test-sandbox"))]
    {
        let _ = cli;
        PathBuf::from("/")
    }
}

/// How a `setup` run ends. Its steps are for a person: standard output, or with `--json`
/// (where standard output is one object) standard error. A failure prints them ahead of
/// its own line, as text, or as the one object.
fn finish_setup(
    json: bool,
    result: Result<(), setup::SetupError>,
    steps: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Exit {
    let shown = String::from_utf8_lossy(steps);
    let Err(e) = result else {
        let _ = if json {
            write!(err, "{shown}")
        } else {
            write!(out, "{shown}")
        };
        return Exit::Ok;
    };
    let _ = if json {
        let object =
            serde_json::json!({"verb": "setup", "error": e.code(), "message": e.to_string()});
        writeln!(out, "{object}")
    } else {
        writeln!(err, "{shown}kbf-installer: setup: {e}")
    };
    e.exit()
}

/// Whether the caller came through `sudo` from another account: sudo sets `SUDO_UID` to the
/// caller's uid and a caller cannot unset it for the command (the test `deploy` makes).
fn sudo_caller() -> bool {
    is_sudo_uid(std::env::var_os("SUDO_UID").as_deref())
}

/// [`sudo_caller`] for a `SUDO_UID` that is `uid`: set, and not root's own.
fn is_sudo_uid(uid: Option<&std::ffi::OsStr>) -> bool {
    uid.is_some_and(|u| u != "0")
}

/// `update`, refused through `sudo`. The verb takes `--api`, `--api-ca`, `--api-server-name`
/// and `--token-file` from its caller, and `--api` is required, so through sudo root would
/// read a file the caller chose and send it to an address the caller chose (a local TLS
/// server with the caller's own CA reads the token from the bearer header). It needs no root:
/// run it as the account that may read the token file.
fn update_verb(
    args: &crate::cli::UpdateArgs,
    via_sudo: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Exit {
    if via_sudo {
        let _ = writeln!(
            err,
            "kbf-installer: update: not accepted through sudo; it needs no root, run it as the account that reads the token file"
        );
        return Exit::Usage;
    }
    crate::deploy::update::run(args, out, err)
}

/// How a `migrate` that went through ends: its report on `out`, as text, or with `--json` as
/// one object. (A failure is said like every other verb's, by `dispatch`.)
fn finish_migrate(json: bool, apply: bool, report: &str, out: &mut dyn Write) -> Exit {
    // A closed pipe is the reader's business; the exit code still tells.
    let _ = if json {
        let object = serde_json::json!({"verb": "migrate", "apply": apply, "message": report});
        writeln!(out, "{object}")
    } else {
        writeln!(out, "{report}")
    };
    Exit::Ok
}

/// Carry out `cli` as the user `euid`. A verb that fails or is not built says so: as text on
/// `err`, or with `--json` as one object on `out`. A verb that succeeds prints nothing here,
/// except `setup` (its steps) and `migrate` (its report).
pub fn dispatch(cli: &Cli, euid: u32, out: &mut dyn Write, err: &mut dyn Write) -> Exit {
    // `update` prints its own summary line and needs no root: it is a client of the update API.
    if let Verb::Update(args) = &cli.verb {
        return update_verb(args, sudo_caller(), out, err);
    }
    let name = cli.verb.name();
    let mut say = |code: Exit, text: String, json: serde_json::Value| {
        // A closed pipe is the reader's business; the exit code still tells.
        let _ = if cli.json {
            writeln!(out, "{json}")
        } else {
            writeln!(err, "{text}")
        };
        code
    };
    // `run` and `boot-check` keep the helper's own checks (they work on a scratch root as an
    // ordinary user) until P5 replaces them; every other verb that is not built is gated
    // here. The verb table says which are built (`built_by`).
    if cli.verb.built_by().is_some() && cli.verb.needs_root() && euid != 0 {
        return say(
            Exit::Usage,
            format!("kbf-installer: {name} needs root: run it with sudo (effective uid is {euid})"),
            serde_json::json!({"verb": name, "error": "needs_root", "euid": euid}),
        );
    }
    // What `backup` and `restore` report when they work; written once `say` is done with `err`.
    let mut said = Vec::new();
    // The old helper and the installer never both manage one host: while the old layout's
    // state is the host's and no migration finished, the verbs that manage it wait.
    // (A verb that is not built yet only says so; the PR that builds it calls this first.)
    if cli.verb.built_by().is_none()
        && cli.verb.manages_host()
        && let Err(f) = old_layout_guard(&root_of(cli))
    {
        return say(
            f.exit,
            format!("kbf-installer: {name}: {}", f.why),
            serde_json::json!({"verb": name, "error": "unmigrated", "message": f.why}),
        );
    }
    if let Verb::Proxy(args) = &cli.verb {
        return match proxy_verb::run(args, &root_of(cli)) {
            Ok(()) => Exit::Ok,
            Err(e) => say(
                e.exit(),
                format!("kbf-installer: {name}: {e}"),
                serde_json::json!({"verb": name, "error": e.code(), "message": e.to_string()}),
            ),
        };
    }
    let result = match &cli.verb {
        // `setup` checks for root itself and prints its steps ahead of its own result.
        Verb::Setup(args) => {
            let mut steps = Vec::new();
            let result = setup::cli(root_of(cli), args, cli.apply, &mut steps);
            return finish_setup(cli.json, result, &steps, out, err);
        }
        Verb::Run(args) => run(root_of(cli), args),
        Verb::BootCheck(args) => boot_check(root_of(cli), args),
        // The report is the result of `migrate`: it is printed here, a failure goes on to
        // be said like every other verb's.
        Verb::Migrate(args) => match migrate_verb(root_of(cli), cli.apply, args) {
            Ok(report) => return finish_migrate(cli.json, cli.apply, &report, out),
            Err(failure) => Err(failure),
        },
        Verb::Confirm(n) => operate(root_of(cli), cli.apply, n.service.restart(), |o| {
            o.confirm(&n.id.to_string())
        }),
        Verb::Revert(n) => operate(root_of(cli), cli.apply, n.service.restart(), |o| {
            o.revert(&n.id.to_string())
        }),
        Verb::Rollback(s) => operate(root_of(cli), cli.apply, s.restart(), |o| o.rollback()),
        Verb::RecordRelease(a) => record_release(root_of(cli), euid, cli.apply, &a.commit),
        Verb::Gc(g) => gc(root_of(cli), cli.apply, g.keep),
        Verb::Quiesce => quiesce(root_of(cli)),
        // The fetch child writes the component to `out` (standard output, a pipe the root
        // parent reads) and says why it could not on `err`, one line. It runs as the fetch
        // user, so no root check applies, and it prints no JSON: its output is the bytes.
        Verb::FetchChild(args) => {
            let written = crate::fetch::child::fetch(args, out).and_then(|n| {
                out.flush().map_err(crate::fetch::child::ChildFault::Out)?;
                Ok(n)
            });
            return match written {
                Ok(_) => Exit::Ok,
                Err(why) => {
                    let _ = writeln!(err, "fetch-child: {why}");
                    Exit::Failed
                }
            };
        }
        Verb::Backup(args) => {
            run_backup(&backup::Process, root_of(cli), cli.apply, args, &mut said)
        }
        Verb::Restore(args) => {
            run_restore(&backup::Process, root_of(cli), cli.apply, args, &mut said)
        }
        other => {
            let phase = other.built_by().unwrap_or("?");
            return say(
                Exit::Failed,
                format!("kbf-installer: {name}: not implemented ({phase})"),
                serde_json::json!({
                    "verb": name,
                    "apply": cli.apply,
                    "error": "not_implemented",
                    "phase": phase,
                }),
            );
        }
    };
    match result {
        Ok(()) => {
            let _ = err.write_all(&said);
            Exit::Ok
        }
        Err(Failure { exit, why }) => say(
            exit,
            format!("kbf-installer: {why}"),
            serde_json::json!({"verb": name, "error": "failed", "message": why}),
        ),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "app_fetch_user_tests.rs"]
mod fetch_user_tests;

/// Start `me` in this process's place with this process's arguments; returns only the error
/// of a start that failed. Not `Command::exec`: the standard library puts SIGPIPE back to its
/// default in the calling process before it execs, and when the exec then fails that process
/// carries on with the default (a write to a pipe nobody reads ends it by a signal instead of
/// failing with an error). `execv` changes no disposition, and the program that starts is
/// this one, which ignores SIGPIPE itself.
fn reexec(me: &Path) -> io::Error {
    // A path and the arguments the kernel passed this process are C strings already, so
    // none holds a NUL byte.
    let arg = |s: &std::ffi::OsStr| CString::new(s.as_bytes()).expect("no NUL byte in an argument");
    let mut argv = vec![arg(me.as_os_str())];
    argv.extend(std::env::args_os().skip(1).map(|a| arg(&a)));
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: `ptrs` is a null-terminated array of pointers to NUL-terminated strings that
    // live (in `argv`) across the call; execv returns only on failure.
    unsafe { libc::execv(ptrs[0], ptrs.as_ptr()) };
    io::Error::last_os_error()
}
