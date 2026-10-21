//! `kbf`: one binary with five modes (docs/design/cluster.md, sections 1 and 5.1), named
//! and mapped to what they run in one table, [`names::MODES`].
//!
//! **The mode is a flag, never a word.** `kbf --mode=frontend|storage|daemon|storage-and-daemon|all-in-one`,
//! or `mode = "..."` in `kbf.toml` (`kbf` alone reads it); a flag overrides the file and the
//! start says so. The old subcommand forms (`kbf frontend`, `kbf storage`, ...) are hidden
//! aliases that run exactly as before, kept until the units and the test farm move
//! (they are the table's rows, so `kbf --help` lists the modes once, under `--mode`).
//! Every other word after `kbf` is refused.
//!
//! - `daemon`: the build node. Takes exactly the flags `kbf-daemon` takes
//!   ([`kbf_node::Cli`]) and refuses, by name, every flag of another mode.
//! - `all-in-one`: the server and the daemon in one process, as `kbf-all-in-one`
//!   runs them today. Takes exactly the flags `kbf-all-in-one` takes
//!   ([`kbf_all_in_one::cli`]).
//! - `storage`: the storage node ([`kbf_server::storage_node`]). Takes
//!   `kbf-server`'s store, write and log flags and its own listener flags, and refuses
//!   every other flag of `kbf-server` (and so any state-store flag: a storage node
//!   reaches the state store only through a frontend).
//! - `frontend`: the REAPI front end ([`kbf_server::frontend`]). Takes `kbf-server`'s
//!   listener, scheduler, state-store and log flags, requires `--storage` and takes the
//!   flags of its mutual TLS to the storage node; refuses every other flag of
//!   `kbf-server` (a frontend holds no stores). It takes `--instance`.
//! - `storage-and-daemon`: the storage node and the daemon in one process, the
//!   daemon reading and writing the node's storage in-process
//!   ([`kbf_all_in_one::storage_and_daemon`]). Takes `storage`'s flags and
//!   `kbf-daemon`'s.
//!
//! **A start with no mode flags reads `kbf.toml`** ([`node`]): `kbf --mode=storage`,
//! `daemon` or `storage-and-daemon` with at most `--config FILE` (default where the
//! packages put it) and `--label`, or `kbf` alone. The start enrolls the host on its first
//! run when the token file is there, for exactly the roles the mode needs, renews without
//! the token on later runs, and runs the mode on the configuration the frontend sent
//! ([`node`], [`enroll`]). `--check` checks that host and exits, starting nothing: 0 for
//! a configured host that is not yet enrolled, 2 for a config, token or enrollment that
//! cannot work. A start that gives mode flags runs on them and does not read the file.
//! There is no `node` mode and no `enroll` word: the host's config states what it is.
//!
//! And words that are not modes: `kbf cluster status|drain|undrain|member|denied|object-store` ([`cluster`]), a
//! client of the frontend's operator API that starts nothing. It is outside
//! [`names::MODES`]; `kbf --help` ends with a line naming it.
//!
//! A mode refuses the flags that belong to another mode ([`refused_by`]): the set is
//! derived from each mode's own flag set, nothing is listed by hand, so a flag added to
//! one mode is refused by the others with no edit here.
//!
//! `kbf --version` prints `kbf <package version>+<commit>`: the commit stamp is
//! `kbf-server`'s ([`kbf_server::SERVER_VERSION`]), which `kbf all-in-one` links
//! anyway, so the artifacts workflow can check `kbf` as it checks `kbf-server`.
//!
//! `kbf --version --json` ([`version`]) prints one JSON object on one line naming the
//! commit, the build profile, the protocol versions and the state schema, so a host can
//! check what it runs.
//!
//! And `kbf frontend handover` ([`handover`]): starts the idle instance of the frontend's
//! systemd template (`kbf-frontend@a`, `kbf-frontend@b`) while the other serves, so the
//! frontend is updated without failing a build. Like `cluster` it starts no mode: `kbf
//! frontend` takes only flags, and the word `handover` after it is this command's.
//!
//! Before a mode starts, [`preflight`] checks what it needs of its host (the files its
//! flags name, its certificate's role, the S3 key pair, the container driver's programs,
//! ...) and refuses, with every missing piece on stderr, one line each, and exit status 2
//! (75 for a piece that heals shortly). `cluster` and `frontend handover` read nothing of
//! the host and skip it.
//!
//! Every mode also takes `--config FILE` and `--check` ([`config`]): the settings may be in
//! `/etc/kbf/kbf.toml` (one TOML table per setting group, one key per flag, a flag on the
//! command line overriding its key), and `--check` parses the flags and the file, runs the
//! startup checks and exits 0, 2 or 75 without starting ([`preflight::check_only`]).
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

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{
    Arg, ArgAction, ArgMatches, Args as _, Command, CommandFactory as _, FromArgMatches as _,
};
use names::{Entry, MODES, Mode};

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
    /// A start that reads `kbf.toml` ([`node`]): `--mode` (or the file's mode) with no
    /// other mode flag.
    Registered(Box<node::Start>),
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
            // The alias word is hidden: the help of a mode shows the flag form.
            let mut cmd = own_command(entry)
                .name(name)
                .bin_name(format!("kbf --mode={name}"))
                .hide(true);
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

/// The whole command line: `kbf --mode=<mode> ...`, the modes of [`names::MODES`]. The
/// modes are also its hidden subcommands (the old forms, `kbf <mode> ...`).
#[must_use]
pub fn command() -> Command {
    let modes: Vec<&'static str> = MODES.iter().map(|m| m.name).collect();
    let mut cmd = Command::new("kbf")
        .version(kbf_server::SERVER_VERSION)
        .about("The kbf build farm.")
        .disable_help_subcommand(true)
        .arg(
            Arg::new("mode")
                .long("mode")
                .value_name("MODE")
                .value_parser(PossibleValuesParser::new(modes))
                .help(
                    "What this process is: `frontend` (the REAPI front end), `storage` (a \
                     storage node), `daemon` (a build node), `storage-and-daemon` (both in \
                     one process) or `all-in-one` (all three). Without it the mode is \
                     `mode` in kbf.toml. `kbf --mode=<mode> --help` lists the mode's flags",
                ),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("FILE")
                .help(format!(
                    "The host's config file, read by a storage, daemon or storage-and-daemon \
                     start that is given no other flag of its mode [default: {}]",
                    config::DEFAULT_PATH
                )),
        )
        .arg(
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue)
                .help(
                    "Check the host for the mode (kbf.toml, the token file or the enrollment) \
                     and exit: 0 when it can start, 2 when a restart cannot fix it; \
                     starts and enrolls nothing",
                ),
        )
        .arg(
            Arg::new("label")
                .long("label")
                .value_name("KEY=VALUE")
                .action(ArgAction::Append)
                .help("A label the daemon half reports to the frontend (repeatable)"),
        );
    for mode in MODES {
        cmd = cmd.subcommand(mode_command(mode.name, mode.entry));
    }
    cmd.after_help(format!(
        "Not a mode: `kbf {CLUSTER} status|drain|undrain|member|denied|object-store` calls a frontend's operator API \
         (`kbf {CLUSTER} --help`); `kbf {} {HANDOVER}` updates a frontend without failing a \
         build (`kbf {} {HANDOVER} --help`).",
        names::FRONTEND,
        names::FRONTEND,
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
/// mode. It is not in [`names::MODES`], so the mode list stays exact.
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

/// `kbf cluster ...`'s command line.
#[must_use]
pub fn cluster_command() -> Command {
    let cmd = Command::new(CLUSTER)
        .bin_name(format!("kbf {CLUSTER}"))
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

/// What the words of a command line say before any mode parses its flags: the mode
/// (`--mode`, or the hidden alias word), `kbf.toml`'s path, `--check` and the labels, and
/// every other word, untouched.
#[derive(Default)]
struct Split {
    alias: Option<OsString>,
    mode: Option<OsString>,
    config: Option<PathBuf>,
    check: bool,
    labels: Vec<OsString>,
    rest: Vec<OsString>,
}

/// A flag that takes a value, as `--name value` or `--name=value`; `None` for another word.
fn value_of<'a>(
    name: &str,
    arg: &str,
    more: &mut impl Iterator<Item = &'a OsString>,
) -> Result<Option<OsString>, clap::Error> {
    let Some(tail) = arg.strip_prefix("--").and_then(|a| a.strip_prefix(name)) else {
        return Ok(None);
    };
    if let Some(value) = tail.strip_prefix('=') {
        return Ok(Some(value.into()));
    }
    if !tail.is_empty() {
        return Ok(None);
    }
    more.next().cloned().map(Some).ok_or_else(|| {
        command().error(
            ErrorKind::InvalidValue,
            format!("a value is required for '--{name}' but none was supplied"),
        )
    })
}

fn split(args: &[OsString]) -> Result<Split, clap::Error> {
    let mut out = Split::default();
    let mut words = args.iter().skip(1);
    let mut first = true;
    while let Some(arg) = words.next() {
        let at_start = std::mem::take(&mut first);
        let Some(text) = arg.to_str() else {
            out.rest.push(arg.clone());
            continue;
        };
        if text == "--" {
            out.rest.push(arg.clone());
            out.rest.extend(words.cloned());
            break;
        } else if at_start && !text.starts_with('-') {
            out.alias = Some(arg.clone());
        } else if text == "--check" {
            out.check = true;
        } else if let Some(value) = value_of("mode", text, &mut words)? {
            out.mode = Some(value);
        } else if let Some(value) = value_of("config", text, &mut words)? {
            out.config = Some(PathBuf::from(value));
        } else if let Some(value) = value_of("label", text, &mut words)? {
            out.labels.push(value);
        } else {
            out.rest.push(arg.clone());
        }
    }
    Ok(out)
}

/// The mode a command line names, by `--mode` or by the hidden alias word.
fn named_mode(split: &Split, args: &[OsString]) -> Result<Option<&'static Mode>, clap::Error> {
    let by_flag = match &split.mode {
        // clap's own refusal, listing the possible values.
        Some(value) => {
            command().try_get_matches_from([
                OsString::from("kbf"),
                "--mode".into(),
                value.clone(),
            ])?;
            MODES.iter().find(|m| value.to_str() == Some(m.name))
        }
        None => None,
    };
    let by_alias = match &split.alias {
        Some(word) => match MODES.iter().find(|m| word.to_str() == Some(m.name)) {
            Some(mode) => Some(mode),
            None => {
                // The word is not a mode: clap's refusal of an unknown subcommand.
                return Err(command()
                    .try_get_matches_from(args)
                    .expect_err("clap refuses a first word that is no mode"));
            }
        },
        None => None,
    };
    if let (Some(flag), Some(alias)) = (by_flag, by_alias) {
        return Err(command().error(
            ErrorKind::ArgumentConflict,
            format!(
                "the mode is given twice: --mode={} and the word {}; give --mode only",
                flag.name, alias.name
            ),
        ));
    }
    Ok(by_flag.or(by_alias))
}

/// Parses a command line (its first word is the program's name) and reads no file unless
/// `--config` names one: [`parse_with`] without a default file.
///
/// # Errors
/// What [`parse_with`] refuses.
pub fn parse_from<I, T>(args: I) -> Result<Invocation, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
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
    /// `--check` was given: print the problems and exit, start nothing. Always false for
    /// [`Invocation::Registered`], which carries its own ([`node::Start::check`]).
    pub check: bool,
}

/// Parses a command line (its first word is the program's name): `kbf --mode=<mode>
/// [flags...]`, or the hidden alias `kbf <mode> [flags...]`, or `kbf` alone with `mode`
/// in the settings file. The settings file is the one `--config` names, else
/// `default_config` when that file exists ([`config::DEFAULT_PATH`] for `kbf`); a flag on
/// the command line overrides the file's key for it.
///
/// A storage, daemon or storage-and-daemon start given no flag of its mode, on a file
/// that holds enrollment's tables, is [`Invocation::Registered`]: the file and the
/// enrollment make its flags ([`node`]).
///
/// # Errors
/// What `clap` refuses, any flag of another mode (which the error names), no mode, and
/// what is wrong with the file ([`config::file_args`]), as an error that exits 2.
pub fn parse_with<I, T>(
    args: I,
    default_config: Option<&std::path::Path>,
) -> Result<Parsed, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    parse_words(args.into_iter().map(Into::into).collect(), default_config)
}

/// [`parse_with`] on the words as the one type, so that the generic wrapper is the only
/// part of it compiled once per argument type (and coverage is measured on one body).
fn parse_words(
    args: Vec<OsString>,
    default_config: Option<&std::path::Path>,
) -> Result<Parsed, clap::Error> {
    let mut split = split(&args)?;
    let mut named = named_mode(&split, &args)?;
    let flag_named = named.is_some();
    let path = split
        .config
        .clone()
        .or_else(|| default_config.filter(|p| p.exists()).map(Into::into));
    if named.is_none() {
        // Flags and no mode: the help and the version are clap's to print.
        let argv = std::iter::once(args[0].clone()).chain(split.rest.iter().cloned());
        if let Err(e) = command().try_get_matches_from(argv) {
            let own_flag = |flag: &str| {
                MODES
                    .iter()
                    .any(|m| longs(&own_command(m.entry)).contains(flag))
            };
            let unknown = split.rest.iter().filter_map(|w| w.to_str()).any(|w| {
                w.strip_prefix("--")
                    .map(|f| f.split('=').next().unwrap_or(f))
                    .is_some_and(|f| !f.is_empty() && !own_flag(f))
            });
            // A flag of no mode is clap's to refuse, naming it; the flags of a mode with
            // no mode named are answered by "no mode" below.
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) || unknown {
                return Err(e);
            }
        }
        named = Some(mode_of_file(path.as_deref(), default_config)?);
    }
    let mode = named.expect("a mode was named or read from the file");
    // A start that gives no flag of its mode reads the host's file; one whose file is not
    // there is refused by that start, naming the file, not by a list of missing flags. A
    // file that exists and has none of enrollment's tables is the mode's flags alone.
    // The old alias word with no `--config` and no `--check` (a `--label` is a flag of the
    // old start) is the old start and wants its flags.
    let bare_alias = split.alias.is_some() && split.config.is_none() && !split.check;
    if split.rest.is_empty()
        && !bare_alias
        && names::is_registered(mode.name)
        && path
            .as_deref()
            .is_none_or(|p| !p.exists() || config::enrolls(p))
    {
        return Ok(Parsed {
            invocation: Invocation::Registered(Box::new(node::Start {
                mode: Some(mode.name),
                config: path.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH)),
                check: split.check,
                labels: split
                    .labels
                    .iter()
                    .map(|l| l.to_string_lossy().into_owned())
                    .collect(),
            })),
            check: false,
        });
    }
    // The flag wins over the file's `mode`; say so (a start that reads the file says it
    // itself, in `node::run_with`).
    if flag_named
        && let Some(path) = &path
        && let Ok(Some(file)) = config::file_mode(path)
        && file != mode.name
    {
        eprintln!(
            "kbf: --mode={} overrides mode = {file:?} in {}",
            mode.name,
            path.display()
        );
    }
    // The mode's own flags: its subcommand word, the labels the split took out put back.
    let labels = split.labels.drain(..).map(|mut l| {
        let mut flag = OsString::from("--label=");
        flag.push(std::mem::take(&mut l));
        flag
    });
    let config = path.as_ref().map(|path| {
        let mut flag = OsString::from("--config=");
        flag.push(path);
        flag
    });
    let argv: Vec<OsString> = [args[0].clone(), mode.name.into()]
        .into_iter()
        .chain(config)
        .chain(split.check.then(|| OsString::from("--check")))
        .chain(labels)
        .chain(split.rest)
        .collect();
    let check = split.check;
    parse_with_file(argv).map(|invocation| Parsed { invocation, check })
}

/// The mode a command line that names none gets from the settings file.
fn mode_of_file(
    path: Option<&std::path::Path>,
    default_config: Option<&std::path::Path>,
) -> Result<&'static Mode, clap::Error> {
    let modes = MODES.map(|m| m.name).join("|");
    let where_ = path.map_or_else(
        || {
            default_config.map_or_else(
                || config::DEFAULT_PATH.to_owned(),
                |p| p.display().to_string(),
            )
        },
        |p| p.display().to_string(),
    );
    let no_mode = |detail: &str| {
        command().error(
            ErrorKind::MissingRequiredArgument,
            format!("no mode: give --mode=<{modes}> or set mode = \"...\" in {where_} ({detail})"),
        )
    };
    let Some(path) = path.filter(|p| p.exists()) else {
        return Err(no_mode("there is no such file"));
    };
    let file = config::file_mode(path).map_err(|why| command().error(ErrorKind::Io, why))?;
    let Some(file) = file else {
        return Err(no_mode("it sets none"));
    };
    MODES.iter().find(|m| m.name == file).ok_or_else(|| {
        command().error(
            ErrorKind::InvalidValue,
            format!("mode = {file:?} in {where_} is not a mode (one of {modes})"),
        )
    })
}

/// The mode of `args` and what a lenient parse of its flags found, when `args` name a
/// mode.
fn lenient(args: &[OsString]) -> Option<(&'static str, Entry, ArgMatches)> {
    let mut lenient = command().ignore_errors(true);
    for mode in MODES {
        lenient = lenient.mut_subcommand(mode.name, |sub| sub.ignore_errors(true));
    }
    let matches = lenient.try_get_matches_from(args).ok()?;
    let (name, m) = matches.subcommand()?;
    let mode = MODES.iter().find(|mode| mode.name == name)?;
    Some((mode.name, mode.entry, m.clone()))
}

/// Checks that every key of the file at `path` is a setting of `mode` (or enrollment's),
/// and that each value is what its flag takes; the flags themselves are not parsed.
///
/// # Errors
/// Every problem of the file, one per line.
pub fn check_settings(mode: &str, path: &std::path::Path) -> Result<(), String> {
    let Some(m) = MODES.iter().find(|m| m.name == mode) else {
        return Err(format!("{mode} is not a mode"));
    };
    config::file_args(
        &own_command(m.entry),
        &refused_own(m.entry),
        path,
        &BTreeSet::new(),
    )
    .map(|_| ())
}

/// Parses `kbf <mode> [flags...]` and the settings file `--config` names in it, if any.
fn parse_with_file(mut args: Vec<OsString>) -> Result<Invocation, clap::Error> {
    if let Some((mode, entry, m)) = lenient(&args)
        && let Some(path) = m.get_one::<PathBuf>(CONFIG).cloned()
    {
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
    parse_invocation(&args)
}

fn parse_invocation(args: &[OsString]) -> Result<Invocation, clap::Error> {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => return Err(refusal_in(args).unwrap_or_else(|| in_flag_form(args, e))),
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

/// `error`, which parsing `kbf <mode> ...` gave, as the flag form says it: the usage line
/// of the mode's help and of its errors names `kbf --mode=<mode>`, not the hidden alias.
/// The mode's command is parsed again as the root of its own command line, which is how
/// clap gets the name it prints; an error the second parse does not repeat is left as it was.
fn in_flag_form(args: &[OsString], error: clap::Error) -> clap::Error {
    let again = args
        .get(1)
        .and_then(|word| MODES.iter().find(|m| word.to_str() == Some(m.name)))
        .and_then(|mode| {
            let argv = args.iter().take(1).chain(args.iter().skip(2));
            mode_command(mode.name, mode.entry)
                .try_get_matches_from(argv)
                .err()
        })
        .filter(|again| again.kind() == error.kind());
    again.unwrap_or(error)
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

/// Starts a parsed command line: the startup checks of the host ([`preflight::refusal`]),
/// and, when they pass, the mode ([`run`]). Every start goes through here, the one that
/// reads `kbf.toml` ([`node::run`]) after it has enrolled included.
fn start(invocation: &Invocation) -> ExitCode {
    preflight::refusal(invocation).unwrap_or_else(|| run(invocation))
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
        Invocation::Registered(start) => node::run(start),
    }
}

/// The `kbf` binary: the daemon's helper re-execs first (a keeper or a `__chown-dirs`
/// run of this binary never reads the command line as `kbf`'s), then [`version::line`]
/// for `kbf --version --json`, then the command line, then [`cluster::run`] for `kbf cluster ...`, [`handover::run`] for `kbf frontend handover ...` or else
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
    if args.get(1).is_some_and(|word| word == names::FRONTEND)
        && args.get(2).is_some_and(|word| word == HANDOVER)
    {
        return match parse_handover(&args) {
            Ok(h) => handover::run(&h),
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
        Ok(Parsed { invocation, .. }) => start(&invocation),
        Err(e) => e.exit(),
    }
}

#[cfg(test)]
mod tests;
