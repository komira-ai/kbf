//! `kbf-installer setup`: the first configuration of a host (docs/design/kbf-installer.md
//! sections 2 and 3, docs/design/distribution.md section 4).
//!
//! It validates its flags, then makes the host what they say: the fetch account (and for the
//! modes that run actions, the `kbf` user's subid range and lingering), the installer's tree
//! with its `orders/` directory, the enrollment token (node modes), the first operator token
//! (frontend and all-in-one hosts, once), `installer.toml`, the first release
//! (`--commit`), `kbf.toml` (both in `/etc/kbf`, `/usr/local/etc/kbf` on a Mac), and last the units enabled for the mode.
//!
//! - It is root's: [`Os::euid`] is checked first, so only a fake that says 0 gets past it
//!   without root. It refuses while the retired `kbf-helper`/`kbf-deploy` layout is the
//!   host's, that is, its state is there and `migrate` has not finished it ([`backend::Migrations`]:
//!   the journal decides, not the old paths, which a migrated host keeps). It retires a unit
//!   file `migrate` repointed where it shadows the mode's packaged unit, keeping its bytes,
//!   and refuses any other file there. Envoy is not its business: the bundled proxy owns
//!   a frontend's port, and a load balancer in front is the operator's.
//! - Files are written by `kbf-host` steps, each a temporary name, fsync, rename and a sync
//!   of the directory. Without `--apply` the steps are printed as `DRY-RUN:` and nothing
//!   changes.
//! - Idempotent: steps are planned only where the host differs. The same flags again is an
//!   empty plan; changed flags rewrite the files atomically and never restart a running
//!   service. An operator floor (`min_generation`) only goes up.
//! - The first release comes through [`Releases`], the installer's one downloader (the
//!   [`Fetcher`](crate::helper::fetcher::Fetcher); the binary's is
//!   [`HostFetcher`](crate::helper::fetcher_host::HostFetcher), which reads `installer.toml`
//!   when asked, so it sees the file this run wrote; a fetch that fails stops `--commit` before
//!   `kbf.toml` with its reason). On a frontend or all-in-one host the
//!   state database's schema is created or brought forward ([`Schemas`]) once `installer.toml`
//!   is written, and a unit that makes a frontend is enabled and never started.
//! - `[proxy] listen` of `installer.toml` (the operator's key) becomes the drop-in
//!   `kbf-proxy.socket.d/listen.conf` on Linux, and no key removes the drop-in. Setup never
//!   reloads systemd or restarts the socket: it says that is needed.
//! - `--release-source bundled` writes `release_source = "bundled"` and fetches nothing: it
//!   takes no `--release-url` and no `--commit`. The keys a source has of its own
//!   (`--release-ca`, the frontend snapshot flags) are checked and written, never read
//!   ([`release_keys`]); the flags decide them, so a run for another source leaves none behind.
//! - `kbf.toml` is written after every file a unit needs, and the units after it.

pub mod backend;
pub mod deploy_config;
pub mod kbf_settings;
pub mod kbf_toml;
pub mod os;
pub mod paths;
pub mod plan;
pub mod provision;
pub mod proxy;
pub mod release_keys;
pub mod validate;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod fetch_user_tests;
#[cfg(test)]
mod kit_adopted_tests;
#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::path::PathBuf;

use kbf_host::{Entry, Exec, HostError, HostExec, HostPath, PathError, Root, Untrusted};

use self::backend::{Backend, Fetching, Journaled, Postgres};
use self::os::{Os, SystemOs};
use self::validate::{Raw, Valid, mode_name};
use crate::cli::{Mode, SetupArgs};
use crate::database::database_step;
use crate::exit::Exit;
use crate::helper::fetcher::FetchError;
use crate::helper::fetcher_host::HostFetcher;
use crate::helper::leases;
use crate::installer_toml::{InstallerToml, Source};
use crate::layout::{Layout, Platform};
use crate::migrate::MigrateError;
use crate::release::ReleaseError;

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
        "the old layout's state is still the host's ({path}) and no migration finished: kbf-installer and the retired kbf-helper/kbf-deploy never both manage a host; run `kbf-installer migrate --apply --fetch-user {fetch_user} [--unit UNIT]` (without --apply it is a dry run), then run setup again"
    )]
    Unmigrated {
        path: String,
        fetch_user: &'static str,
    },
    /// The migration's journal or the old layout could not be read.
    #[error("{0}")]
    Migrate(#[from] MigrateError),
    #[error(
        "{path} is a regular file and shadows the packaged unit {unit} (systemd reads /etc before /usr/lib, so enabling would keep its old flags): move it aside (mv {path} {path}.old; systemctl daemon-reload) and run setup again; setup never deletes a unit"
    )]
    ShadowUnit { path: String, unit: &'static str },
    #[error("{0}")]
    Fetch(#[from] FetchError),
    #[error("{0}")]
    Release(#[from] ReleaseError),
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
            SetupError::Fetch(FetchError::Rejected(_)) => Exit::Refused,
            SetupError::NeedsOperator(_)
            | SetupError::Unmigrated { .. }
            | SetupError::ShadowUnit { .. } => Exit::NeedsOperator,
            SetupError::Migrate(e) => e.exit(),
            SetupError::SubIdSpace(_)
            | SetupError::NoFreeId
            | SetupError::Fetch(_)
            | SetupError::Release(_)
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
            SetupError::Unmigrated { .. } => "unmigrated",
            SetupError::Migrate(_) => "migrate",
            SetupError::ShadowUnit { .. } => "unit_shadow",
            SetupError::Fetch(FetchError::NotWired) => "not_implemented",
            SetupError::Fetch(_) => "fetch",
            SetupError::Release(_) => "release",
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
/// keys the flags give are put in it ([`proxy::ProxyPlan::merge`]).
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
    settings.source = v.source.clone();
    // The flags decide the keys of the source: a key of a source the host no longer has is
    // not kept, or the file would not read back.
    settings.release_ca = v.release_ca.clone();
    settings.frontend = v.frontend.clone();
    settings.database = v.database.clone();
    (settings.node_cert, settings.node_key) = match (&v.source, node_role(v.mode)) {
        (Source::Frontend, Some(role)) => {
            let (cert, key) = paths::node_identity(platform, role);
            (Some(PathBuf::from(cert)), Some(PathBuf::from(key)))
        }
        _ => (None, None),
    };
    if let Some(plan) = &v.proxy {
        settings.proxy = Some(plan.merge(settings.proxy.take())?);
    }
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

/// Whether anything is at `path`: a link counts, whether or not it points at something.
fn present(exec: &dyn Exec, path: &HostPath) -> Result<bool, SetupError> {
    Ok(exec.stat(path)?.is_some() || exec.inspect(path)?.is_some())
}

/// The old layout's state is the host's until a migration finished: the journal says so, not
/// the paths (a migrated host keeps `/usr/local/kbf` and `/var/lib/kbf-deploy` as its
/// rollback path). Returns the units that migration repointed.
fn migrated_units(
    backend: &Backend<'_>,
    platform: Platform,
) -> Result<Vec<crate::migrate::Unit>, SetupError> {
    backend.migrations.units().map_err(|e| match e {
        MigrateError::Unmigrated(path) => SetupError::Unmigrated {
            path,
            fetch_user: plan::fetch_user(platform),
        },
        other => SetupError::Migrate(other),
    })
}

/// A regular file in `/etc/systemd/system` with the name of the mode's packaged unit that
/// `setup` retires: the unit file of the old layout, which a finished migration repointed.
pub struct Shadow {
    pub path: String,
    pub unit: &'static str,
    pub bytes: Vec<u8>,
}

/// Where a regular file in `/etc/systemd/system` has the name of the mode's packaged unit:
/// systemd reads `/etc` before `/usr/lib`, and `setup` does not enable a unit that is already
/// enabled, so the host would go on running the old file's flags. A link is what
/// `systemctl link` makes and is not a shadow with other flags.
///
/// A file the migration journalled by name is the old layout's unit and is returned, to be
/// retired with its bytes kept. Any other is refused, naming it: `setup` never removes a unit
/// it did not see the migration take over.
fn find_shadows(
    exec: &dyn Exec,
    mode: Mode,
    journalled: &[crate::migrate::Unit],
    proxy: bool,
) -> Result<Vec<Shadow>, SetupError> {
    let mut found = Vec::new();
    for unit in plan::mode_units(mode, proxy) {
        let path = format!("/etc/systemd/system/{unit}");
        let Some(Entry::File { bytes, .. }) = exec.inspect(&HostPath::new(&path)?)? else {
            continue;
        };
        if !journalled.iter().any(|j| j.as_str() == *unit) {
            return Err(SetupError::ShadowUnit { path, unit });
        }
        found.push(Shadow { path, unit, bytes });
    }
    Ok(found)
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
    if options.apply {
        os.trusted_binary()?;
    }
    let valid = raw.validate(os.platform()).map_err(SetupError::Invalid)?;
    let mut ctx = plan::context(os, valid.mode)?;
    let journalled = migrated_units(backend, ctx.platform)?;
    ctx.kit_adopted = backend.migrations.kit_adopted()?;
    let shadows = if ctx.platform == Platform::Linux {
        find_shadows(&*exec, valid.mode, &journalled, valid.proxy.is_some())?
    } else {
        Vec::new()
    };
    let service_uid = ctx.service.as_ref().map(|u| u.uid);
    kbf_settings::check_files(&*exec, &valid.kbf, service_uid)?;
    if let Some(proxy) = &valid.proxy {
        kbf_settings::check_files(&*exec, &proxy.files(), service_uid)?;
    }
    let proxy = match valid.deploy {
        Some(_) => installer_settings(&*exec, &valid, ctx.platform)?.proxy,
        None => None,
    };
    let deploy = deploy_config::resolve(&*exec, valid.deploy.as_ref(), proxy.as_ref(), &ctx)?;
    let mut notes = Vec::new();

    let before = plan::before_release(os, &*exec, &valid, &ctx)?;
    let mut steps = emit(&before.plan, exec, options.apply, out)?;
    notes.extend(before.notes);

    // The migrated unit file stands in the way of the packaged unit: it is taken out, a copy
    // kept in the migration's backup first, and the packaged unit is enabled below.
    if !shadows.is_empty() {
        let mark = if options.apply { "+" } else { "DRY-RUN:" };
        for shadow in &shadows {
            let path = &shadow.path;
            let line = format!("{mark} retire {path} (shadows the packaged unit; original kept)");
            writeln!(out, "{line}")?;
        }
        let retire = plan::retire(&*exec, &ctx, &shadows)?;
        steps += emit(&retire, exec, options.apply, out)?;
        notes.push(
            "retired unit files are kept in the migration's backup; `kbf-installer migrate --rollback` puts them back"
                .to_owned(),
        );
    }

    // The schema is made after `installer.toml` is in place and before anything is fetched or
    // enabled: a database that cannot be reached stops setup with no release and no unit.
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

    let mut have_release = released(&*exec, &ctx.layout)?;
    match (&valid.commit, have_release) {
        (Some(commit), false) => {
            steps += 1;
            if options.apply {
                writeln!(out, "+ fetch and verify release {commit}")?;
                // It returns only once the release is installed, verified and current.
                backend.releases.install_first(commit)?;
                have_release = true;
            } else {
                writeln!(out, "DRY-RUN: fetch and verify release {commit}")?;
                have_release = true;
            }
        }
        (Some(commit), true) => notes.push(format!(
            "a release is installed already: --commit {commit} is not fetched (setup installs the first release only; use update)"
        )),
        (None, _) => {}
    }

    // `deploy.json` is a file like `kbf.toml`: written before the units that need a host that has it.
    if let Some(config) = &deploy {
        let written = deploy_config::plan(&*exec, &ctx, config)?;
        steps += emit(&written.plan, exec, options.apply, out)?;
        notes.extend(written.notes);
    }

    let retired: Vec<&str> = shadows.iter().map(|s| s.unit).collect();
    let after = plan::after_release(
        os,
        &*exec,
        &valid,
        &ctx,
        have_release,
        options.no_start,
        &retired,
    )?;
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

/// The fetcher of the first release (`--commit`): the host's own, which reads
/// `installer_config` (else the platform's own `installer.toml`) when it is asked.
///
/// Its lease source is [`NotWired`](crate::helper::leases::NotWired), which is never asked:
/// the setup fetch is `Fetcher::fetch`, a frontend update, and the verify pipeline skips the
/// lease step for that kind (a host that has never run its unit has no cgroup to read).
fn first_release_fetcher(layout: Layout, installer_config: Option<HostPath>) -> HostFetcher {
    let installer_toml = installer_config.unwrap_or_else(|| layout.installer_toml());
    HostFetcher::on_host(layout, installer_toml, || Box::new(leases::NotWired))
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
    let raw = Raw {
        mode: args.mode,
        frontend_url: args.frontend_url.clone(),
        token_file: args.token_file.clone(),
        release_source: args.release_source,
        release_url: args.release_url.clone(),
        release: args.release.clone(),
        min_generation: args.min_generation,
        commit: args.commit.clone(),
        database: args.database.clone(),
        kbf: args.kbf.clone(),
        proxy: args.proxy.clone(),
        create_local_database: args.create_local_database,
        deploy: args.deploy.clone(),
        installer_config: args.installer_config.as_ref().map(ToString::to_string),
    };
    let options = Options {
        apply,
        no_start: args.no_start,
    };
    // The first release is fetched after `installer.toml` is written and the fetch user made,
    // both by this run, so the fetcher reads them when `--commit` asks, not now.
    let layout = Layout::new(Root::new(&root)?, platform);
    let fetcher = first_release_fetcher(layout.clone(), args.installer_config.clone());
    let releases = Fetching::new(&fetcher, layout, HostExec::new(Root::new(&root)?));
    let journal = Journaled::new(
        Layout::new(Root::new(&root)?, platform),
        crate::app::owner_uid(root != std::path::Path::new("/"), exec.euid()),
    );
    let backend = Backend {
        releases: &releases,
        schemas: &Postgres,
        migrations: &journal,
    };
    run(&os, &mut exec, &backend, &raw, options, out)
}
