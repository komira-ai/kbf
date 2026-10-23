//! `kbf-installer setup`: the first configuration of a host (docs/design/kbf-installer.md
//! sections 2 and 3, docs/design/distribution.md section 4).
//!
//! It validates its flags, then makes the host what they say: the fetch account (and for the
//! modes that run actions, the `kbf` user's subid range and lingering), the installer's tree
//! with its `orders/` directory, the enrollment token (node modes), the first operator token
//! (frontend and all-in-one hosts, once), `installer.toml`, `kbf.toml` (both in `/etc/kbf`,
//! `/usr/local/etc/kbf` on a Mac), and last the units enabled for the mode.
//!
//! - It is root's: [`Os::euid`] is checked first, so only a fake that says 0 gets past it
//!   without root. It refuses a regular file in `/etc/systemd/system` that shadows the
//!   mode's packaged unit. Envoy is not its business: the bundled proxy owns
//!   a frontend's port, and a load balancer in front is the operator's.
//! - Files are written by `kbf-host` steps, each a temporary name, fsync, rename and a sync
//!   of the directory. Without `--apply` the steps are printed as `DRY-RUN:` and nothing
//!   changes.
//! - Idempotent: steps are planned only where the host differs. The same flags again is an
//!   empty plan; changed flags rewrite the files atomically and never restart a running
//!   service. An operator floor (`min_generation`) only goes up.
//! - It installs no release: kbf updates are OS packages. A unit is started only where `kbf` is
//!   there already (a release at `releases/current`, or `/usr/bin/kbf` from the kbf package),
//!   and is otherwise enabled and left stopped. On a
//!   frontend or all-in-one host the state database's schema is created or brought forward
//!   ([`Schemas`]) once `installer.toml` is written, and a unit that makes a frontend is enabled
//!   and never started.
//! - `--mode=all-in-one` is always behind the proxy (`--proxy`): the host runs the proxy, its
//!   frontend children and `kbf-storage-and-daemon.service` beside them. `kbf.toml` is the
//!   sibling's (`mode = "storage-and-daemon"` on the loopback `--storage-port`) and
//!   `frontend.toml`, which `installer.toml` names as `frontend_config`, the children's; the
//!   second is written first. A `kbf.toml` that says `mode = "all-in-one"` is the earlier layout
//!   (one process for everything): `setup` splits it into these files, keeping the original, and
//!   names what the new layout has no place for, but stops and starts none of the services of
//!   the move; it prints the commands that finish the move ([`aio_migrate`]).
//! - `[proxy] listen` of `installer.toml` (the operator's key) becomes the drop-in
//!   `kbf-proxy.socket.d/listen.conf` on Linux, and no key removes the drop-in. Setup never
//!   reloads systemd or restarts the socket: it says that is needed.
//! - It writes no release source and none of the keys that go with one: `installer.toml` keeps
//!   the `release_source` and the keys a host set up by an earlier build has, and a host with no
//!   file gets the default source. The flags that named them (`--release-source`,
//!   `--release-url`, `--release-ca`, the frontend snapshot flags) and `--commit` are taken for
//!   one more release and ignored, with one warning ([`retired`]).
//! - `kbf.toml` is written after every file a unit needs, and the units after it.

pub mod aio_migrate;
pub mod backend;
pub mod kbf_settings;
pub mod kbf_toml;
pub mod os;
pub mod paths;
pub mod plan;
pub mod provision;
pub mod proxy;
pub mod retired;
pub mod validate;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod fetch_user_tests;
#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::path::PathBuf;

use kbf_host::{Entry, Exec, HostError, HostExec, HostPath, PathError, Root, Untrusted};

use self::backend::{Backend, Postgres};
use self::os::{Os, SystemOs};
use self::validate::{Raw, Valid, mode_name};
use crate::cli::{Mode, SetupArgs};
use crate::database::database_step;
use crate::exit::Exit;
use crate::installer_toml::{InstallerToml, Source};
use crate::layout::{Layout, Platform};

/// Why setup stopped.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("setup writes /etc and manages services: run it with sudo (effective uid is {euid})")]
    NotRoot { euid: u32 },
    #[error("refusing to run as root from a binary root does not own alone: {0}")]
    Untrusted(#[from] Untrusted),
    #[error("{}", .0.join("\n"))]
    Invalid(Vec<String>),
    #[error("token file {0}")]
    Token(String),
    #[error("{0}")]
    Unsupported(String),
    #[error(
        "no user {name}: the package creates it (systemd-sysusers); run it, or reinstall the package"
    )]
    NoUser { name: &'static str },
    #[error("no room for a {0} range of 65536 ids above the existing ones")]
    SubIdSpace(&'static str),
    #[error("no free id between 300 and 399 for the fetch account")]
    NoFreeId,
    #[error(
        "{path} is a regular file and shadows the packaged unit {unit} (systemd reads /etc before /usr/lib, so enabling would keep its old flags): move it aside (mv {path} {path}.old; systemctl daemon-reload) and run setup again; setup never deletes a unit"
    )]
    ShadowUnit { path: String, unit: &'static str },
    #[error("database step: {0}")]
    Database(String),
    /// Only a person can go on: the reason, and what to do about it.
    #[error("{0}")]
    NeedsOperator(String),
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Host(#[from] HostError),
}

impl SetupError {
    /// The exit code (contract section 2).
    pub fn exit(&self) -> Exit {
        match self {
            SetupError::NotRoot { .. }
            | SetupError::Untrusted(_)
            | SetupError::Invalid(_)
            | SetupError::Token(_)
            | SetupError::Unsupported(_)
            | SetupError::NoUser { .. } => Exit::Usage,
            SetupError::NeedsOperator(_) | SetupError::ShadowUnit { .. } => Exit::NeedsOperator,
            SetupError::SubIdSpace(_)
            | SetupError::NoFreeId
            | SetupError::Database(_)
            | SetupError::Path(_)
            | SetupError::Io(_)
            | SetupError::Host(_) => Exit::Failed,
        }
    }

    /// A stable word for `--json`.
    pub fn code(&self) -> &'static str {
        match self {
            SetupError::NotRoot { .. } => "needs_root",
            SetupError::Untrusted(_) => "untrusted_binary",
            SetupError::Invalid(_) => "invalid_flags",
            SetupError::Token(_) => "token_file",
            SetupError::Unsupported(_) => "unsupported",
            SetupError::NoUser { .. } => "no_user",
            SetupError::SubIdSpace(_) | SetupError::NoFreeId => "no_room",
            SetupError::NeedsOperator(_) => "needs_operator",
            SetupError::ShadowUnit { .. } => "unit_shadow",
            SetupError::Database(_) => "database",
            SetupError::Path(_) | SetupError::Io(_) | SetupError::Host(_) => "failed",
        }
    }
}

/// What `setup` was asked to do besides the flags.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Carry the plan out (`--apply`); otherwise print it.
    pub apply: bool,
    /// Enable the units without starting them.
    pub no_start: bool,
}

/// The `installer.toml` this run leaves: the file that is there with the flags' settings put
/// in, or the defaults with them. A file that is not valid is not overwritten (the operator
/// decides), and a floor never goes down. Its `[proxy]` table is kept; on a `--proxy` host the
/// keys the flags give are put in it ([`proxy::ProxyPlan::merge`]). The release source and the
/// keys that go with it are not the flags' to set: the file's own are kept, and a host with no
/// file gets the default source.
pub fn installer_settings(
    exec: &dyn Exec,
    v: &Valid,
    platform: Platform,
) -> Result<InstallerToml, SetupError> {
    let installer_toml = paths::installer_toml(platform);
    let path = HostPath::new(installer_toml)?;
    let mut settings = match exec.inspect(&path)? {
        None => InstallerToml::default(),
        Some(Entry::File { bytes, .. }) => {
            let text = String::from_utf8_lossy(&bytes);
            InstallerToml::parse(&text).map_err(|e| {
                SetupError::NeedsOperator(format!(
                    "{e}\nfix {installer_toml} or remove it, then run setup again"
                ))
            })?
        }
        Some(_) => {
            return Err(SetupError::NeedsOperator(format!(
                "{installer_toml} is not a regular file"
            )));
        }
    };
    if let Some(floor) = v.min_generation {
        if floor < settings.min_generation {
            return Err(SetupError::NeedsOperator(format!(
                "--min-generation {floor} is below the {} in {}: a floor only goes up; lower it by editing the file",
                settings.min_generation, installer_toml
            )));
        }
        settings.min_generation = floor;
    }
    settings.database = v.database.clone();
    // The node identity belongs to the frontend source, whichever way the file got it.
    (settings.node_cert, settings.node_key) = match (&settings.source, node_role(v.mode)) {
        (Source::Frontend, Some(role)) => {
            let (cert, key) = paths::node_identity(platform, role);
            (Some(PathBuf::from(cert)), Some(PathBuf::from(key)))
        }
        _ => (None, None),
    };
    if let Some(plan) = &v.proxy {
        settings.proxy = Some(plan.merge(settings.proxy.take())?);
    }
    // The flags decide this key as they decide the rest: the proxy's children read their own
    // file only where a sibling holds `kbf.toml`.
    settings.frontend_config = v
        .storage_port()
        .map(|_| PathBuf::from(paths::frontend_toml(platform)));
    Ok(settings)
}

/// The role whose certificate and key the fetch child is lent: the daemon's if the host has
/// one, else the storage node's (contract section 3).
fn node_role(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Daemon | Mode::StorageAndDaemon => Some("daemon"),
        Mode::Storage => Some("storage"),
        Mode::Frontend | Mode::AllInOne => None,
    }
}

/// Whether a release is installed: `releases/current` is there (a link counts).
fn released(exec: &dyn Exec, layout: &Layout) -> Result<bool, SetupError> {
    present(exec, &layout.current())
}

/// The `kbf` the kbf package installs, which every mode unit runs.
const PACKAGED_KBF: &str = "/usr/bin/kbf";

/// Whether the host has the `kbf` the kbf package installs: a host from the packages has no
/// release tree, and this file is what its units run. A link counts, as it does for
/// [`released`]: only its being there is asked.
fn packaged_kbf(exec: &dyn Exec) -> Result<bool, SetupError> {
    present(exec, &HostPath::new(PACKAGED_KBF)?)
}

/// Whether anything is at `path`: a link counts, whether or not it points at something.
fn present(exec: &dyn Exec, path: &HostPath) -> Result<bool, SetupError> {
    Ok(exec.stat(path)?.is_some() || exec.inspect(path)?.is_some())
}

/// Where a regular file in `/etc/systemd/system` has the name of the mode's packaged unit:
/// systemd reads `/etc` before `/usr/lib`, and `setup` does not enable a unit that is already
/// enabled, so the host would go on running the old file's flags. A link is what
/// `systemctl link` makes and is not a shadow with other flags. Refused, naming the file:
/// `setup` never removes a unit it did not write.
fn find_shadows(exec: &dyn Exec, mode: Mode, proxy: bool) -> Result<(), SetupError> {
    for unit in plan::mode_units(mode, proxy) {
        let path = format!("/etc/systemd/system/{unit}");
        if let Some(Entry::File { .. }) = exec.inspect(&HostPath::new(&path)?)? {
            return Err(SetupError::ShadowUnit { path, unit });
        }
    }
    Ok(())
}

/// Print the steps of `plan` and, when applying, carry them out. Returns how many there were.
fn emit(
    plan: &kbf_host::Plan,
    exec: &mut dyn Exec,
    apply: bool,
    out: &mut dyn Write,
) -> Result<usize, SetupError> {
    for (index, step) in plan.steps().iter().enumerate() {
        if apply {
            writeln!(out, "+ {step}")?;
            exec.apply(step).map_err(|source| HostError::Step {
                index,
                step: step.to_string(),
                source,
            })?;
        } else {
            writeln!(out, "DRY-RUN: {step}")?;
        }
    }
    Ok(plan.steps().len())
}

/// Validate, plan and (with `options.apply`) carry out `setup` for `raw` on the host `os` and
/// `exec` describe, printing each step and each note (never a secret).
pub fn run(
    os: &dyn Os,
    exec: &mut dyn Exec,
    backend: &Backend<'_>,
    raw: &Raw,
    options: Options,
    out: &mut dyn Write,
) -> Result<(), SetupError> {
    let euid = os.euid();
    if euid != 0 {
        return Err(SetupError::NotRoot { euid });
    }
    if let Some(warning) = raw.retired.warning() {
        writeln!(out, "{warning}")?;
    }
    if options.apply {
        os.trusted_binary()?;
    }
    // A host of the earlier layout is moved: the flags this run does not give come from its
    // `kbf.toml`. That is read before the flags are checked, and a run without `--proxy` (the
    // command line that set the host up) is told it is that host, not that `--proxy` is required.
    let earlier = match raw.mode {
        Mode::AllInOne => aio_migrate::read(&*exec, os.platform())?,
        _ => None,
    };
    let mut adopted = raw.clone();
    if let Some(earlier) = &earlier {
        earlier.adopt(&mut adopted)?;
    }
    let moving = earlier.is_some();
    let valid = adopted
        .validate(os.platform())
        .map_err(|problems| aio_migrate::invalid(problems, moving))?;
    let mut ctx = plan::context(os, valid.mode)?;
    if let Some(earlier) = &earlier {
        earlier.check(&*exec, &ctx)?;
    }
    ctx.earlier = earlier;
    if ctx.platform == Platform::Linux {
        find_shadows(&*exec, valid.mode, valid.proxy.is_some())?;
    }
    let service_uid = ctx.service.as_ref().map(|u| u.uid);
    let mut problems = Vec::new();
    kbf_settings::push_problems(&*exec, &valid.kbf, service_uid, &mut problems);
    // The proxy's files belong to the proxy's user, which on an all-in-one host is not the
    // sibling's `service`. Both are checked before either is reported.
    if let Some(proxy) = &valid.proxy {
        let frontend = ctx.frontend_uid();
        kbf_settings::push_problems(&*exec, &proxy.files(), frontend, &mut problems);
    }
    if !problems.is_empty() {
        return Err(aio_migrate::invalid(problems, moving));
    }
    let mut notes = Vec::new();

    let before = plan::before_release(os, &*exec, &valid, &ctx)?;
    let mut steps = emit(&before.plan, exec, options.apply, out)?;
    notes.extend(before.notes);

    // The schema is made after `installer.toml` is in place and before `kbf.toml` is written or
    // anything is enabled: a database that cannot be reached stops setup with no unit.
    if let Some(url) = &valid.database {
        let created = match &valid.local_database {
            Some(local) => provision::provision(os, exec, local, options.apply, out)?,
            None => provision::Next::Schema,
        };
        if created == provision::Next::Pending {
            writeln!(
                out,
                "kbf-installer setup ({}): database step: dry run: the database does not exist yet, so its schema would be created after it",
                mode_name(valid.mode)
            )?;
        } else {
            let schema = backend.schemas.of(url)?;
            schema
                .reach()
                .map_err(|why| provision::unreached(&why, valid.mode))?;
            if let Some(local) = &valid.local_database {
                provision::check_service_login(os, &*exec, local, valid.mode)?;
            }
            let line = database_step(&*schema, mode_name(valid.mode), options.apply)
                .map_err(SetupError::Database)?;
            writeln!(out, "{line}")?;
        }
    }

    // No release is installed here. A unit has a `kbf` to run when a release is installed
    // already or the kbf package is.
    let runs = released(&*exec, &ctx.layout)? || packaged_kbf(&*exec)?;
    let after = plan::after_release(os, &*exec, &valid, &ctx, runs, options.no_start)?;
    steps += emit(&after.plan, exec, options.apply, out)?;
    notes.extend(after.notes);

    if steps == 0 {
        writeln!(out, "{}", kbf_host::plan::NOTHING_TO_DO)?;
    } else if !options.apply {
        writeln!(out, "{}", kbf_host::plan::DRY_RUN_NOTE)?;
    }
    for note in &notes {
        writeln!(out, "note: {note}")?;
    }
    Ok(())
}

/// The verb as the binary runs it, on the host under `root`.
pub fn cli(
    root: PathBuf,
    args: &SetupArgs,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), SetupError> {
    let platform = Platform::current();
    let mut exec = HostExec::new(Root::new(&root)?);
    // `os` reads through its own copy of the host, so `run` can change the real one.
    let reader = exec.clone();
    let os = SystemOs::new(&reader, platform);
    let raw = raw_from(args);
    let options = Options {
        apply,
        no_start: args.no_start,
    };
    let schemas = Postgres;
    let backend = Backend { schemas: &schemas };
    run(&os, &mut exec, &backend, &raw, options, out)
}

/// The command line's values as `run` takes them, unchecked.
pub fn raw_from(args: &SetupArgs) -> Raw {
    Raw {
        mode: args.mode,
        frontend_url: args.frontend_url.clone(),
        token_file: args.token_file.clone(),
        min_generation: args.min_generation,
        retired: args.retired.clone(),
        database: args.database.clone(),
        kbf: args.kbf.clone(),
        proxy: args.proxy.clone(),
        create_local_database: args.create_local_database,
        installer_config: args.installer_config.as_ref().map(ToString::to_string),
    }
}
