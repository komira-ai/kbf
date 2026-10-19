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
use kbf_state::postgres::schema::Outcome;

use crate::activate;
use crate::cli::{BootArgs, Cli, RunArgs, SetupArgs, Verb};
use crate::database::Database;
use crate::exit::Exit;
use crate::helper::cycle::{Config, Helper, HelperError};
use crate::helper::operator::{self, Operator};
use crate::helper::restart::Restart;
use crate::helper::serve::{FileIdentity, Flow, PASS_INTERVAL, Serve};
use crate::helper::service::HostProcs;
use crate::helper::{fetcher, leases};
use crate::layout::{Layout, Platform};
use crate::proxy_verb;
use crate::release::ReleaseError;

/// Why a verb failed and the exit it ends with: `Failed`, or `NeedsOperator` when only an
/// operator can go on (the state files are both unusable).
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

/// The database step of `setup`, for the modes that keep their state in the database: a dry
/// run reports the schema's version and what would be applied; `--apply` creates the schema
/// or brings an older one forward, and does nothing when it is current. Returns the line to
/// print; it says it covers the database step only, because the rest of `setup` is not built.
/// It needs no root: it talks to the database as the role the URL names.
fn setup_database(root: &Path, apply: bool, args: &SetupArgs) -> Result<String, Failure> {
    let database = Database::from_installer_toml(&under(root, &args.installer_config)?)
        .map_err(|e| Failure::from(e.to_string()))?;
    database_step(&database, &args.mode.name(), apply)
}

/// What the database step needs of the schema: `Database` is the one real implementation; a
/// test's fake stands in for a server, so the lines `setup` prints after the schema runner
/// answers are tested without one. The errors are their text: that is all `setup` prints.
trait SchemaOps {
    /// Creates or brings forward the schema.
    fn setup(&self) -> Result<Outcome, String>;

    /// The schema's version (`None` before the first setup) and this build's newest.
    fn plan(&self) -> Result<(Option<u32>, u32), String>;
}

impl SchemaOps for Database {
    fn setup(&self) -> Result<Outcome, String> {
        Database::setup(self).map_err(|e| e.to_string())
    }

    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        Database::plan(self).map_err(|e| e.to_string())
    }
}

/// The line the database step prints for `mode`: `--apply` runs the schema, a dry run asks.
fn database_step(database: &dyn SchemaOps, mode: &str, apply: bool) -> Result<String, Failure> {
    if apply {
        let outcome = database.setup().map_err(Failure::from)?;
        return Ok(apply_line(mode, &outcome));
    }
    let (found, newest) = database.plan().map_err(Failure::from)?;
    Ok(dry_run_line(mode, found, newest))
}

/// The line `setup --apply` prints for what the database step did.
fn apply_line(mode: &str, outcome: &Outcome) -> String {
    format!("kbf-installer setup ({mode}): database step: {outcome}")
}

/// The line a `setup` dry run prints: the schema's version (`None` before the first setup)
/// against this build's newest.
fn dry_run_line(mode: &str, found: Option<u32>, newest: u32) -> String {
    match found {
        None => format!(
            "kbf-installer setup ({mode}): database step: dry run: would create the schema at version {newest}"
        ),
        Some(v) if v < newest => format!(
            "kbf-installer setup ({mode}): database step: dry run: would migrate the schema from {v} to {newest}"
        ),
        Some(v) => format!(
            "kbf-installer setup ({mode}): database step: dry run: the schema is at {v}, \
             nothing to do"
        ),
    }
}

fn run(root: PathBuf, args: &RunArgs) -> Result<(), Failure> {
    // A running program has a file, and the file can be read: neither can be told otherwise.
    let me = std::env::current_exe().expect("a running program has a file");
    let watch = FileIdentity::of(&me).expect("the file I run from can be read");
    run_from(root, args, &me, watch)
}

/// The helper `run` drives: the host's, with the schema step of `--installer-config` attached
/// when the flag is given.
fn build_helper(root: PathBuf, args: &RunArgs) -> Result<Helper<HostExec>, Failure> {
    let database = match &args.installer_config {
        Some(config) => Some(
            Database::from_installer_toml(&under(&root, config)?)
                .map_err(|e| Failure::from(e.to_string()))?,
        ),
        None => None,
    };
    let (layout, exec) = host(root)?;
    let platform = layout.platform();
    let cfg = Config {
        update_dir: args.update_dir.clone(),
        orders_dir: args.orders_dir.clone(),
        restart: args.service.restart(),
        window_secs: args.window_secs,
        starts: args.starts,
        min_activation_interval_secs: args.min_activation_interval_secs,
    };
    // The release fetcher and the lease source are wired by later changes; until then an
    // activation of a release that is not installed, and every activation, is refused.
    let mut helper = Helper::new(
        layout,
        exec,
        cfg,
        Box::new(fetcher::NotWired),
        Box::new(leases::NotWired),
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
    if args.installer_config.is_none() && args.service.runs_database_mode() {
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

/// The success line of the one verb that reports one (the database step of `setup`): text
/// on `out`, or with `--json` one object.
fn print_success(json: bool, apply: bool, verb: &str, line: &str, out: &mut dyn Write) {
    // A closed pipe is the reader's business; the exit code still tells.
    let _ = if json {
        let object = serde_json::json!({"verb": verb, "apply": apply, "message": line});
        writeln!(out, "{object}")
    } else {
        writeln!(out, "{line}")
    };
}

/// Carry out `cli` as the user `euid`. A verb that fails or is not built says so: as text on
/// `err`, or with `--json` as one object on `out`. A verb that succeeds prints nothing here,
/// except the database step of `setup`, whose one line goes to `out`.
pub fn dispatch(cli: &Cli, euid: u32, out: &mut dyn Write, err: &mut dyn Write) -> Exit {
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
    if let Verb::Proxy(args) = &cli.verb {
        return match proxy_verb::run(args) {
            Ok(()) => Exit::Ok,
            Err(e) => say(
                e.exit(),
                format!("kbf-installer: {name}: {e}"),
                serde_json::json!({"verb": name, "error": e.code(), "message": e.to_string()}),
            ),
        };
    }
    let result = match &cli.verb {
        // As the role the database URL names, the step needs no root.
        Verb::Setup(args) if args.mode.keeps_state_in_database() => {
            setup_database(&root_of(cli), cli.apply, args).map(Some)
        }
        Verb::Run(args) => run(root_of(cli), args).map(|()| None),
        Verb::BootCheck(args) => boot_check(root_of(cli), args).map(|()| None),
        Verb::Confirm(n) => operate(root_of(cli), cli.apply, n.service.restart(), |o| {
            o.confirm(&n.id.to_string())
        })
        .map(|()| None),
        Verb::Revert(n) => operate(root_of(cli), cli.apply, n.service.restart(), |o| {
            o.revert(&n.id.to_string())
        })
        .map(|()| None),
        Verb::Rollback(s) => {
            operate(root_of(cli), cli.apply, s.restart(), |o| o.rollback()).map(|()| None)
        }
        Verb::Gc(g) => gc(root_of(cli), cli.apply, g.keep).map(|()| None),
        Verb::Quiesce => quiesce(root_of(cli)).map(|()| None),
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
        Ok(line) => report(line, cli, name, out),
        Err(Failure { exit, why }) => say(
            exit,
            format!("kbf-installer: {why}"),
            serde_json::json!({"verb": name, "error": "failed", "message": why}),
        ),
    }
}

/// A verb that succeeded: its line, when it has one, goes to `out`.
fn report(line: Option<String>, cli: &Cli, name: &str, out: &mut dyn Write) -> Exit {
    if let Some(line) = line {
        print_success(cli.json, cli.apply, name, &line, out);
    }
    Exit::Ok
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

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
