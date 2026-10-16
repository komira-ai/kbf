//! `kbf`: one binary with five modes (docs/design/cluster.md, sections 1 and 5.1), named
//! and mapped to what they run in one table, [`names::MODES`].
//!
//! - `kbf daemon`: the build node. Takes exactly the flags `kbf-daemon` takes
//!   ([`kbf_node::Cli`]) and refuses, by name, every flag of another mode.
//! - `kbf all-in-one`: the server and the daemon in one process, as `kbf-all-in-one`
//!   runs them today. Takes exactly the flags `kbf-all-in-one` takes
//!   ([`kbf_all_in_one::cli`]).
//! - `kbf frontend`, `kbf storage`, `kbf storage-and-daemon`: not built yet. Each
//!   exits 2 with one line naming the mode.
//!
//! A mode refuses the flags that belong to another mode ([`refused_by`]): the set is
//! derived from each mode's own flag set, nothing is listed by hand, so a flag added to
//! one mode is refused by the others with no edit here.
//!
//! `kbf --version` prints `kbf <package version>+<commit>`: the commit stamp is
//! `kbf-server`'s ([`kbf_server::SERVER_VERSION`]), which `kbf-all-in-one` links
//! anyway, so the artifacts workflow can check `kbf` as it checks `kbf-server`.
//!
//! The `kbf-server`, `kbf-daemon` and `kbf-all-in-one` binaries are unchanged.

pub mod names;

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::OnceLock;

use clap::builder::Resettable;
use clap::error::ErrorKind;
use clap::{Arg, ArgMatches, Command, CommandFactory as _, FromArgMatches as _};
use names::{Entry, MODES};

/// A parsed command line. The flags are boxed: the variants differ a lot in size.
#[derive(Clone, Debug)]
pub enum Invocation {
    /// `kbf daemon`.
    Daemon(Box<kbf_node::Cli>),
    /// `kbf all-in-one`.
    AllInOne(Box<kbf_all_in_one::cli::Flags>),
    /// A mode that is not built yet.
    NotBuilt {
        /// The mode.
        mode: &'static str,
    },
}

fn longs(cmd: &Command) -> BTreeSet<String> {
    cmd.get_arguments()
        .filter_map(|a| a.get_long().map(str::to_owned))
        .collect()
}

/// The command line a mode with an entry point parses; none for an unbuilt mode.
fn own_command(entry: Entry) -> Option<Command> {
    match entry {
        Entry::Daemon => Some(kbf_node::Cli::command()),
        Entry::AllInOne => Some(kbf_all_in_one::cli::command()),
        Entry::NotBuilt => None,
    }
}

/// The long names of the flags mode `mode` refuses: every flag of another mode's own
/// command line that its own does not take (flags that modes share stay accepted).
/// Empty for a mode with no command line of its own (an unbuilt one, which refuses
/// everything by exiting 2) and for a mode that takes the union.
#[must_use]
pub fn refused_by(mode: &str) -> &'static [&'static str] {
    static REFUSED: OnceLock<BTreeMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    let all = REFUSED.get_or_init(|| {
        let own: Vec<(&'static str, BTreeSet<String>)> = MODES
            .iter()
            .filter_map(|m| own_command(m.entry).map(|c| (m.name, longs(&c))))
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
        Entry::Daemon | Entry::AllInOne => {
            let mut cmd = own_command(entry)
                .expect("a mode with an entry point has a command line")
                .name(name)
                .bin_name(Resettable::<String>::Reset);
            for flag in refused_by(name) {
                // The id is the flag's own name: no argument of either has a hyphen in its id.
                cmd = cmd.arg(Arg::new(*flag).long(*flag).num_args(0..=1).hide(true));
            }
            cmd
        }
        Entry::NotBuilt => Command::new(name).about("Not built yet").arg(
            Arg::new("args")
                .num_args(0..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true)
                .hide(true),
        ),
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
    cmd
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
    let args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let matches = match command().try_get_matches_from(&args) {
        Ok(matches) => matches,
        Err(e) => return Err(refusal_in(&args).unwrap_or(e)),
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
        Entry::NotBuilt => Ok(Invocation::NotBuilt { mode: mode.name }),
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

/// Runs a parsed command line: the mode's own entry point, or for a mode not built yet
/// one line on stderr and exit 2.
#[must_use]
pub fn run(invocation: &Invocation) -> ExitCode {
    match invocation {
        Invocation::Daemon(cli) => kbf_node::run(cli),
        Invocation::AllInOne(flags) => kbf_all_in_one::run(flags),
        Invocation::NotBuilt { mode } => {
            eprintln!("{}", names::not_built_line(mode));
            ExitCode::from(2)
        }
    }
}

/// The `kbf` binary: the daemon's helper re-execs first (a keeper or a `__chown-dirs`
/// run of this binary never reads the command line as `kbf`'s), then the command line,
/// then [`run`]. A command line that does not parse exits 2 (0 for `--help` and
/// `--version`).
#[must_use]
pub fn main() -> ExitCode {
    if let Some(code) = kbf_node::dispatch_helpers() {
        return code;
    }
    match parse_from(std::env::args_os()) {
        Ok(invocation) => run(&invocation),
        Err(e) => e.exit(),
    }
}

#[cfg(test)]
mod tests;
