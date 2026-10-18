//! `kbf`: one binary with five modes (docs/design/cluster.md, sections 1 and 5.1), named
//! and mapped to what they run in one table, [`names::MODES`].
//!
//! - `kbf daemon`: the build node. Takes exactly the flags `kbf-daemon` takes
//!   ([`kbf_node::Cli`]) and refuses, by name, every flag of another mode.
//! - `kbf all-in-one`: the server and the daemon in one process, as `kbf-all-in-one`
//!   runs them today. Takes exactly the flags `kbf-all-in-one` takes
//!   ([`kbf_all_in_one::cli`]).
//! - `kbf storage`: the storage node ([`kbf_server::storage_node`]). Takes
//!   `kbf-server`'s store, write and log flags and its own listener flags, and refuses
//!   every other flag of `kbf-server` (and so any state-store flag: a storage node
//!   reaches the state store only through a frontend).
//! - `kbf frontend`: the REAPI front end ([`kbf_server::frontend`]). Takes `kbf-server`'s
//!   listener, scheduler, state-store and log flags, requires `--storage` and takes the
//!   flags of its mutual TLS to the storage node; refuses every other flag of
//!   `kbf-server` (a frontend holds no stores).
//! - `kbf storage-and-daemon`: the storage node and the daemon in one process, the
//!   daemon reading and writing the node's storage in-process
//!   ([`kbf_all_in_one::storage_and_daemon`]). Takes `kbf storage`'s flags and
//!   `kbf-daemon`'s.
//!
//! And one word that is not a mode: `kbf cluster status|drain|undrain` ([`cluster`]), a
//! client of the frontend's operator API that starts nothing. Like `check-args` it is
//! outside [`names::MODES`]; `kbf --help` ends with a line naming it.
//!
//! A mode refuses the flags that belong to another mode ([`refused_by`]): the set is
//! derived from each mode's own flag set, nothing is listed by hand, so a flag added to
//! one mode is refused by the others with no edit here.
//!
//! `kbf --version` prints `kbf <package version>+<commit>`: the commit stamp is
//! `kbf-server`'s ([`kbf_server::SERVER_VERSION`]), which `kbf-all-in-one` links
//! anyway, so the artifacts workflow can check `kbf` as it checks `kbf-server`.
//!
//! `kbf --version --json` ([`version`]) prints one JSON object on one line naming the
//! commit, the build profile, the protocol versions and the state schema, so a host can
//! check what it runs.
//!
//! `kbf check-args <mode> [flags...]` ([`check_args`]) parses `kbf <mode> [flags...]`
//! as a start would, then exits, starting nothing: 0 when the mode would start with
//! those flags, 2 with clap's message when it would refuse them (and for a mode not
//! built yet, which refuses every start). `kbf-deploy linux check-units --mode` runs
//! it on a unit's `ExecStart=`. It is not a mode, so `kbf --help` does not list it.
//!
//! And `kbf frontend handover` ([`handover`]): starts the idle instance of the frontend's
//! systemd template (`kbf-frontend@a`, `kbf-frontend@b`) while the other serves, so the
//! frontend is updated without failing a build. Like `cluster` it starts no mode: `kbf
//! frontend` takes only flags, and the word `handover` after it is this command's.
//!
//! Before a mode starts, [`preflight`] checks what it needs of its host (the files its
//! flags name, its certificate's role, the S3 key pair, the container driver's programs,
//! ...) and refuses, with every missing piece on stderr, one line each, and exit status 2
//! (75 for a piece that heals shortly). `check-args`, `cluster` and `frontend handover`
//! read nothing of the host and skip it.
//!
//! Every mode also takes `--config FILE` and `--check` ([`config`]): the settings may be in
//! `/etc/kbf/kbf.toml` (one TOML table per setting group, one key per flag, a flag on the
//! command line overriding its key), and `--check` parses the flags and the file, runs the
//! startup checks and exits 0, 2 or 75 without starting ([`preflight::check_only`]).
//! `check-args` is `--check`'s parse stage only, kept for `kbf-deploy`, with a deprecation
//! note on its last line of standard error.
//!
//! And the product's two words for a node ([`node`], [`enroll`]): `kbf enroll` enrolls the
//! node with the frontend its `kbf.toml` names (one token, one key per role, a certificate
//! from the frontend's CA) and exits; `kbf node` does that if the node is not enrolled,
//! then runs the mode its granted roles name (`storage`, `daemon` or `storage-and-daemon`)
//! and renews the certificates while it runs. Like `cluster` they are outside
//! [`names::MODES`]; `kbf check-args node ...` and `kbf check-args enroll ...` accept
//! their flags.
//!
//! The `kbf-server`, `kbf-daemon` and `kbf-all-in-one` binaries are unchanged.

pub mod cluster;
pub mod config;
pub mod enroll;
pub mod handover;
pub mod names;
pub mod node;
pub mod nofile;
pub mod preflight;
pub mod version;

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::OnceLock;

use clap::builder::Resettable;
use clap::error::ErrorKind;
use clap::{Arg, ArgMatches, Args as _, Command, CommandFactory as _, FromArgMatches as _};
use names::{Entry, MODES};

/// A parsed command line. The flags are boxed: the variants differ a lot in size.
#[derive(Clone, Debug)]
pub enum Invocation {
    /// `kbf daemon`.
    Daemon(Box<kbf_node::Cli>),
    /// `kbf all-in-one`.
    AllInOne(Box<kbf_all_in_one::cli::Flags>),
    /// `kbf storage`.
    Storage(Box<kbf_server::storage_node::StorageFlags>),
    /// `kbf storage-and-daemon`.
    StorageAndDaemon(Box<kbf_all_in_one::storage_and_daemon::Flags>),
    /// `kbf frontend`.
    Frontend(Box<kbf_server::frontend::FrontendFlags>),
}

fn longs(cmd: &Command) -> BTreeSet<String> {
    cmd.get_arguments()
        .filter_map(|a| a.get_long().map(str::to_owned))
        .collect()
}

/// The command line a mode parses.
fn own_command(entry: Entry) -> Command {
    match entry {
        Entry::Daemon => kbf_node::Cli::command(),
        Entry::AllInOne => kbf_all_in_one::cli::command(),
        Entry::Storage => kbf_server::storage_node::command(),
        Entry::StorageAndDaemon => kbf_all_in_one::storage_and_daemon::command(),
        Entry::Frontend => kbf_server::frontend::command(),
    }
}

/// The flags a mode's own command carries only so that it can refuse them by name (the
/// flags of `kbf-server` that `kbf storage` and `kbf frontend` do not take): they are
/// no settings.
fn refused_own(entry: Entry) -> BTreeSet<String> {
    match entry {
        Entry::Frontend => kbf_server::frontend::refused_flags(),
        Entry::Storage => kbf_server::storage_node::refused_flags(),
        // The daemon's flags are its own whatever they are called.
        Entry::StorageAndDaemon => {
            let daemon = longs(&kbf_node::Cli::command());
            kbf_server::storage_node::refused_flags()
                .into_iter()
                .filter(|flag| !daemon.contains(flag))
                .collect()
        }
        Entry::Daemon | Entry::AllInOne => BTreeSet::new(),
    }
}

/// The long names of the flags mode `mode` refuses: every flag of another mode's own
/// command line that its own does not take (flags that modes share stay accepted).
/// Empty for a mode that takes the union.
#[must_use]
pub fn refused_by(mode: &str) -> &'static [&'static str] {
    static REFUSED: OnceLock<BTreeMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    let all = REFUSED.get_or_init(|| {
        let own: Vec<(&'static str, BTreeSet<String>)> = MODES
            .iter()
            .map(|m| (m.name, longs(&own_command(m.entry))))
            .collect();
        own.iter()
            .map(|(name, mine)| {
                let foreign: BTreeSet<&String> = own
                    .iter()
                    .filter(|(other, _)| other != name)
                    .flat_map(|(_, flags)| flags)
                    .filter(|f| !mine.contains(*f))
                    .collect();
                // Leaked once per process: clap wants names that live as long as the command.
                let leaked = foreign
                    .into_iter()
                    .map(|s| &*Box::leak(s.clone().into_boxed_str()))
                    .collect();
                (*name, leaked)
            })
            .collect()
    });
    all.get(mode).map_or(&[], Vec::as_slice)
}

/// Why `kbf <mode>` refuses `--flag`.
fn reason(mode: &str, flag: &str) -> String {
    if mode == names::DAEMON
        && ["store", "tls", "token", "auth"]
            .iter()
            .any(|w| flag.contains(w))
    {
        "a build node holds no store and no credential (docs/design/cluster.md, section 3.3)"
            .to_owned()
    } else {
        format!("the flag belongs to another mode, which `kbf {mode}` does not take")
    }
}

fn mode_command(name: &'static str, entry: Entry) -> Command {
    match entry {
        Entry::Daemon
        | Entry::AllInOne
        | Entry::Storage
        | Entry::StorageAndDaemon
        | Entry::Frontend => {
            let mut cmd = own_command(entry)
                .name(name)
                .bin_name(Resettable::<String>::Reset);
            for flag in refused_by(name) {
                // The id is the flag's own name: no argument of either has a hyphen in its id.
                cmd = cmd.arg(Arg::new(*flag).long(*flag).num_args(0..=1).hide(true));
            }
            cmd.arg(
                Arg::new(CONFIG)
                    .long(CONFIG)
                    .value_name("FILE")
                    .value_parser(clap::value_parser!(std::path::PathBuf))
                    .help(format!(
                        "Read the mode's settings from this TOML file (default {} when it \
                         exists); a flag on the command line overrides its key",
                        config::DEFAULT_PATH
                    )),
            )
            .arg(
                Arg::new(CHECK)
                    .long(CHECK)
                    .action(clap::ArgAction::SetTrue)
                    .help(
                        "Parse the flags and the file, run the startup checks, print each \
                         problem and exit (0 fine, 2 refused, 75 not ready) without starting",
                    ),
            )
        }
    }
}

/// The whole command line: `kbf <mode> ...`, the modes of [`names::MODES`].
#[must_use]
pub fn command() -> Command {
    let mut cmd = Command::new("kbf")
        .version(kbf_server::SERVER_VERSION)
        .about("The kbf build farm.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true);
    for mode in MODES {
        cmd = cmd.subcommand(mode_command(mode.name, mode.entry));
    }
    cmd.after_help(format!(
        "Not a mode: `kbf {CLUSTER} status|drain|undrain` calls a frontend's operator API \
         (`kbf {CLUSTER} --help`); `kbf {} {HANDOVER}` updates a frontend without failing a \
         build (`kbf {} {HANDOVER} --help`); `kbf {NODE}` enrolls this machine and runs \
         the mode its granted roles name (`kbf {NODE} --help`); `kbf {ENROLL}` only enrolls \
         (`kbf {ENROLL} --help`).",
        names::FRONTEND,
        names::FRONTEND,
        NODE = names::NODE,
        ENROLL = names::ENROLL,
    ))
}

/// The whole `kbf.toml` of mode `mode`, every setting in its group's table
/// ([`config::template`]); `None` for a word that is not a mode.
#[must_use]
pub fn config_template(mode: &str) -> Option<String> {
    MODES
        .iter()
        .find(|m| m.name == mode)
        .map(|m| config::template(m.name, &own_command(m.entry), &refused_own(m.entry)))
}

/// The word after `kbf` that calls the operator API ([`cluster`]) instead of starting a
/// mode. Like [`CHECK_ARGS`] it is not in [`names::MODES`], so the mode list stays exact.
pub const CLUSTER: &str = "cluster";

/// The word after `kbf frontend` that starts the idle instance of the frontend's unit
/// instead of the mode ([`handover`]).
pub const HANDOVER: &str = "handover";

/// `kbf frontend handover ...`'s command line.
#[must_use]
pub fn handover_command() -> Command {
    let cmd = Command::new(HANDOVER)
        .bin_name(format!("kbf {} {HANDOVER}", names::FRONTEND))
        .about(
            "Start the idle instance of kbf-frontend@.service while the other serves, so the \
             frontend is updated without failing a build.",
        );
    handover::Handover::augment_args(cmd)
}

/// Parses `kbf frontend handover ...`; `args` is the whole command line, its second word
/// [`names::FRONTEND`] and its third [`HANDOVER`].
///
/// # Errors
/// What `clap` refuses.
pub fn parse_handover(args: &[std::ffi::OsString]) -> Result<handover::Handover, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(3));
    let matches = handover_command().try_get_matches_from(argv)?;
    handover::Handover::from_arg_matches(&matches)
}

/// `kbf node ...`'s command line.
#[must_use]
pub fn node_command() -> Command {
    let cmd = Command::new(names::NODE)
        .bin_name(format!("kbf {}", names::NODE))
        .about(
            "Enroll this machine with its frontend if it is not enrolled, run the mode its \
             granted roles name (storage, daemon, or both) and renew its certificates.",
        );
    node::Node::augment_args(cmd)
}

/// Parses `kbf node ...`; `args` is the whole command line, its second word
/// [`names::NODE`].
///
/// # Errors
/// What `clap` refuses.
pub fn parse_node(args: &[std::ffi::OsString]) -> Result<node::Node, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    let matches = node_command().try_get_matches_from(argv)?;
    node::Node::from_arg_matches(&matches)
}

/// `kbf enroll ...`'s command line.
#[must_use]
pub fn enroll_command() -> Command {
    let cmd = Command::new(names::ENROLL)
        .bin_name(format!("kbf {}", names::ENROLL))
        .about("Enroll this machine with its frontend, once, and exit.");
    enroll::Enroll::augment_args(cmd)
}

/// Parses `kbf enroll ...`; `args` is the whole command line, its second word
/// [`names::ENROLL`].
///
/// # Errors
/// What `clap` refuses.
pub fn parse_enroll(args: &[std::ffi::OsString]) -> Result<enroll::Enroll, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    let matches = enroll_command().try_get_matches_from(argv)?;
    enroll::Enroll::from_arg_matches(&matches)
}

/// `kbf cluster ...`'s command line.
#[must_use]
pub fn cluster_command() -> Command {
    let cmd = Command::new(CLUSTER)
        .bin_name(format!("kbf {CLUSTER}"))
        .about("Drain, undrain and show the cluster through the frontend's operator API.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true);
    cluster::Cluster::augment_args(cmd)
}

/// Parses `kbf cluster ...`; `args` is the whole command line, its first word the
/// program's name and its second [`CLUSTER`].
///
/// # Errors
/// What `clap` refuses.
pub fn parse_cluster(args: &[std::ffi::OsString]) -> Result<cluster::Cluster, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    let matches = cluster_command().try_get_matches_from(argv)?;
    cluster::Cluster::from_arg_matches(&matches)
}

/// Parses a command line (its first word is the program's name).
///
/// # Errors
/// What `clap` refuses, and any flag of another mode, which the error names.
pub fn parse_from<I, T>(args: I) -> Result<Invocation, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    parse_with(args, None).map(|parsed| parsed.invocation)
}

/// The flag that names the settings file.
pub const CONFIG: &str = "config";

/// The flag that checks a start instead of making it.
pub const CHECK: &str = "check";

/// A command line parsed with its settings file.
#[derive(Clone, Debug)]
pub struct Parsed {
    /// The mode and its flags: the command line's, then the file's for those it did not give.
    pub invocation: Invocation,
    /// `--check` was given: print the problems and exit, start nothing.
    pub check: bool,
}

/// Parses a command line (its first word is the program's name) and the settings file it
/// names with `--config`, else `default_config` when that file exists
/// ([`config::DEFAULT_PATH`] for `kbf <mode>`): [`parse_from`] reads no file unless
/// `--config` names one.
///
/// # Errors
/// What [`parse_from`] refuses, and what is wrong with the file ([`config::file_args`]),
/// as an error that exits 2.
pub fn parse_with<I, T>(
    args: I,
    default_config: Option<&std::path::Path>,
) -> Result<Parsed, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    let mut args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let mut check = false;
    if let Some((mode, entry, m)) = lenient(&args) {
        check = m.get_flag(CHECK);
        let path = m
            .get_one::<std::path::PathBuf>(CONFIG)
            .cloned()
            .or_else(|| default_config.filter(|p| p.exists()).map(Into::into));
        if let Some(path) = path {
            let own = own_command(entry);
            let given = own
                .get_arguments()
                .map(|a| a.get_id().as_str())
                .filter(|id| m.value_source(id) == Some(clap::parser::ValueSource::CommandLine))
                .map(str::to_owned)
                .collect();
            let extra = config::file_args(&own, &refused_own(entry), &path, &given)
                .map_err(|why| command().error(ErrorKind::Io, format!("kbf {mode}: {why}")))?;
            args.extend(extra);
        }
    }
    parse_invocation(&args).map(|invocation| Parsed { invocation, check })
}

/// The mode of `args` and what a lenient parse of its flags found, when `args` name a
/// mode.
fn lenient(args: &[std::ffi::OsString]) -> Option<(&'static str, Entry, ArgMatches)> {
    let mut lenient = command().ignore_errors(true);
    for mode in MODES {
        lenient = lenient.mut_subcommand(mode.name, |sub| sub.ignore_errors(true));
    }
    let matches = lenient.try_get_matches_from(args).ok()?;
    let (name, m) = matches.subcommand()?;
    let mode = MODES.iter().find(|mode| mode.name == name)?;
    Some((mode.name, mode.entry, m.clone()))
}

fn parse_invocation(args: &[std::ffi::OsString]) -> Result<Invocation, clap::Error> {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => return Err(refusal_in(args).unwrap_or(e)),
    };
    let (name, m) = matches.subcommand().expect("a subcommand is required");
    let mode = MODES
        .iter()
        .find(|mode| mode.name == name)
        .expect("clap matched a mode of the table");
    match mode.entry {
        Entry::Daemon => {
            refuse_foreign_flags(mode.name, m)?;
            Ok(Invocation::Daemon(Box::new(
                kbf_node::Cli::from_arg_matches(m)?,
            )))
        }
        Entry::AllInOne => {
            refuse_foreign_flags(mode.name, m)?;
            Ok(Invocation::AllInOne(Box::new(kbf_all_in_one::cli::parse(
                m,
            )?)))
        }
        Entry::Storage => {
            refuse_foreign_flags(mode.name, m)?;
            Ok(Invocation::Storage(Box::new(
                kbf_server::storage_node::parse(m)?,
            )))
        }
        Entry::StorageAndDaemon => {
            refuse_foreign_flags(mode.name, m)?;
            Ok(Invocation::StorageAndDaemon(Box::new(
                kbf_all_in_one::storage_and_daemon::parse(m)?,
            )))
        }
        Entry::Frontend => {
            refuse_foreign_flags(mode.name, m)?;
            Ok(Invocation::Frontend(Box::new(kbf_server::frontend::parse(
                m,
            )?)))
        }
    }
}

/// The refusal a mode owes for `args`, if they hold a flag of another mode, found even
/// when clap fails on the way (a build node given only `--cold-store` would otherwise
/// hear that its required flags are missing, not that the flag is not its own).
fn refusal_in(args: &[std::ffi::OsString]) -> Option<clap::Error> {
    // Clap reads the parent's setting when a subcommand's parse fails, and the mode's
    // own for its arguments.
    let mut lenient = command().ignore_errors(true);
    for mode in MODES {
        lenient = lenient.mut_subcommand(mode.name, |sub| sub.ignore_errors(true));
    }
    let matches = lenient.try_get_matches_from(args).ok()?;
    let (name, m) = matches.subcommand()?;
    refuse_foreign_flags(name, m).err()
}

/// Fails, naming the first flag of [`refused_by`]`(mode)` that `m` holds.
fn refuse_foreign_flags(mode: &str, m: &ArgMatches) -> Result<(), clap::Error> {
    match refused_by(mode)
        .iter()
        .find(|flag| m.value_source(flag).is_some())
    {
        Some(flag) => Err(command().error(
            ErrorKind::ArgumentConflict,
            format!("kbf {mode} refuses --{flag}: {}", reason(mode, flag)),
        )),
        None => Ok(()),
    }
}

/// Runs a parsed command line: the mode's own entry point.
#[must_use]
pub fn run(invocation: &Invocation) -> ExitCode {
    match invocation {
        Invocation::Daemon(cli) => kbf_node::run(cli),
        Invocation::AllInOne(flags) => kbf_all_in_one::run(flags),
        Invocation::Storage(flags) => kbf_server::storage_node::run(flags),
        Invocation::StorageAndDaemon(flags) => kbf_all_in_one::storage_and_daemon::run(flags),
        Invocation::Frontend(flags) => kbf_server::frontend::run(flags),
    }
}

/// The word after `kbf` that checks a command line instead of starting it
/// ([`check_args`]).
pub const CHECK_ARGS: &str = "check-args";

/// `kbf check-args <mode> [flags...]`: parses `kbf <mode> [flags...]` as [`main`]
/// would, and starts nothing (the parse reads no file and opens no socket). `args` is
/// the whole command line, its first word the program's name and its second
/// [`CHECK_ARGS`].
///
/// # Errors
/// What [`parse_from`] refuses, and `--help` or `--version`, which would print and exit 0 without
/// starting the mode.
pub fn check_args(args: &[std::ffi::OsString]) -> Result<(), clap::Error> {
    // `kbf <word> [flags...]`, as the start would be given.
    let argv = args
        .iter()
        .take(1)
        .chain(args.iter().skip(2))
        .cloned()
        .collect::<Vec<_>>();
    let parsed = match argv.get(1).and_then(|word| word.to_str()) {
        Some(names::NODE) => parse_node(&argv).map(drop),
        Some(names::ENROLL) => parse_enroll(&argv).map(drop),
        _ => parse_from(argv).map(drop),
    };
    match parsed {
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            Err(command().error(
                ErrorKind::ArgumentConflict,
                "--help and --version print and exit 0: the mode would not start",
            ))
        }
        other => other,
    }
}

/// The `kbf` binary: the daemon's helper re-execs first (a keeper or a `__chown-dirs`
/// run of this binary never reads the command line as `kbf`'s), then [`version::line`]
/// for `kbf --version --json`, then the command line, then [`check_args`] for `kbf
/// check-args ...`, [`cluster::run`] for `kbf cluster ...`, [`handover::run`] for `kbf frontend handover ...` or else
/// [`preflight::refusal`] and [`run`]. A command
/// line that
/// does not parse exits 2 (0 for `--help` and `--version`).
#[must_use]
pub fn main() -> ExitCode {
    if let Some(code) = kbf_node::dispatch_helpers() {
        return code;
    }
    // A failure leaves the limit as it was: the process still starts, and the first
    // EMFILE names the cause.
    let _ = nofile::raise();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if version::asked(&args) {
        print!("{}", version::line());
        return ExitCode::SUCCESS;
    }
    if args.get(1).is_some_and(|word| word == CHECK_ARGS) {
        let code = match check_args(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = e.print();
                ExitCode::from(2)
            }
        };
        // After the result: `kbf-deploy` reports the first line this prints.
        eprintln!(
            "kbf: `check-args` is deprecated: it only parses the flags; use `kbf <mode> ... \
             --check`, which also reads the config file and runs the startup checks"
        );
        return code;
    }
    if args.get(1).is_some_and(|word| word == names::FRONTEND)
        && args.get(2).is_some_and(|word| word == HANDOVER)
    {
        return match parse_handover(&args) {
            Ok(h) => handover::run(&h),
            Err(e) => e.exit(),
        };
    }
    if args.get(1).is_some_and(|word| word == names::NODE) {
        return match parse_node(&args) {
            Ok(node) => node::run(&node),
            Err(e) => e.exit(),
        };
    }
    if args.get(1).is_some_and(|word| word == names::ENROLL) {
        return match parse_enroll(&args) {
            Ok(enroll) => enroll::run(&enroll),
            Err(e) => e.exit(),
        };
    }
    if args.get(1).is_some_and(|word| word == CLUSTER) {
        return match parse_cluster(&args) {
            Ok(cluster) => cluster::run(&cluster),
            Err(e) => e.exit(),
        };
    }
    match parse_with(args, Some(std::path::Path::new(config::DEFAULT_PATH))) {
        Ok(Parsed {
            invocation,
            check: true,
        }) => preflight::check_only(&invocation),
        Ok(Parsed { invocation, .. }) => {
            preflight::refusal(&invocation).unwrap_or_else(|| run(&invocation))
        }
        Err(e) => e.exit(),
    }
}

#[cfg(test)]
mod tests;
