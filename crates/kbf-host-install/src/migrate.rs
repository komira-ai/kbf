//! `kbf-host-install --migrate-to-package`: move a Linux daemon host from the hand-placed
//! layout (`/var/lib/kbf-installer/releases/<commit>`, a unit in `/etc/systemd/system` that
//! runs `releases/current/kbf` with the node's flags) to the `kbf` package, once, by an
//! operator, as root.
//!
//! ```text
//! kbf-host-install --migrate-to-package --layout deb|rpm --channel NAME --unit UNIT --dir DIR [--apply]
//! ```
//!
//! It plans by default: the plan is printed, `DRY-RUN:` before each step, and nothing on the
//! host changes. `--apply` carries it out. The checks that come first run in both and change
//! nothing: the host's installer state names the running commit and has no release on trial,
//! the unit is one this program can carry over ([`unit::parse`]), and the package for that very
//! commit in `--dir` (the three files of the release: `kbf-<commit>-linux-<cpu>.<deb|rpm>`,
//! its `.sigstore.json` bundle and `SHA256SUMS`) passes every check a package passes before an
//! update installs it ([`PackageHost::check_copies`]), and is the commit the host runs. A deb is
//! then unpacked in the checks' own directory and its `kbf` is run on the flags the drop-in
//! gives it, `--help` last, so that it reads them and starts nothing; the directories the node's
//! flags name (the data, metadata and local cold-store directories) are recorded with their mode
//! and owner, and a data directory the container runtime cannot reach (some directory down to it
//! closed to others) is refused. The plan prints what it found as `checked:` lines.
//!
//! The steps, in order:
//!
//! 1. the package manager installs the verified file (the units it ships are disabled and sit
//!    under `/usr/lib/systemd/system`, behind the hand-placed unit in `/etc/systemd/system`,
//!    so nothing changes for the running node), and any recorded directory the install changed
//!    gets its mode and owner back;
//! 2. a drop-in `/etc/systemd/system/<unit>.d/10-node-flags.conf` is written: the node's flags
//!    on the packaged unit's `ExecStart=` and `--check`, its account, and the few settings
//!    named in [`unit::HandPlaced`], the packaged unit's `kbf.toml` condition lifted (the
//!    flags are the node's configuration) and its `StateDirectory=` cleared (systemd would set
//!    `/var/lib/kbf` to 0750 at every start; the node's directories are the node's). The mode
//!    is named by `--mode=` alone: the older first word (`kbf storage-and-daemon ...`) is dropped
//!    ([`unit::HandPlaced::flags`]);
//! 3. the hand-placed unit is renamed `<unit>.hand-placed`, so the packaged one takes its
//!    place, and systemd rereads its files;
//! 4. the unit restarts and is waited for;
//! 5. the check: the unit's main process runs `/usr/bin/kbf`, that binary reports the
//!    commit the host ran, and the recorded directories have the mode and owner they had;
//! 6. only then is the old layout (`releases/`) removed.
//!
//! A failure after step 1 puts the hand-placed unit back (the drop-in is removed, systemd
//! rereads, the recorded directories get their mode and owner back, the unit restarts and is
//! watched for [`STAY`]: one that does not stay active is reported with the state it was seen
//! in) and leaves the old layout alone; the package stays installed.

mod unit;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use kbf_installer::state;
use serde_json::Value;

use crate::local::backend::Fault;
use crate::local::package::{Copies, PackageHost};
use crate::local::package_format::Format;
use crate::local::units::restart_and_wait;
use crate::mac::release::{self, commit_prefix};
use crate::pace::Pace;
use crate::plan::{self, SYSTEMCTL, Wait};
use crate::request::{Channel, Unit};
use crate::runner::{Cmd, Identity, Runner};

pub const USAGE: &str = "usage: kbf-host-install --migrate-to-package --layout deb|rpm \
--channel NAME --unit UNIT --dir DIR [--apply]\n";

/// The units of the modes a daemon host runs. (A frontend is not migrated here.)
const MODES: [&str; 4] = ["daemon", "storage", "storage-and-daemon", "all-in-one"];

/// The name of the drop-in.
const DROP_IN: &str = "10-node-flags.conf";

/// The suffix the hand-placed unit is kept under.
const ASIDE: &str = ".hand-placed";

/// The packaged binary.
const KBF: &str = "/usr/bin/kbf";

/// The modes whose process runs containers, and so keeps a data directory the container
/// runtime has to reach.
const RUNS_CONTAINERS: [&str; 3] = ["daemon", "storage-and-daemon", "all-in-one"];

/// Where a daemon keeps its data without `--data-dir`.
const DEFAULT_DATA_DIR: &str = "/var/lib/kbf";

/// How long the hand-placed unit, put back, must stay active before it is called running: a
/// unit that crashes at once and is restarted (`Restart=on-failure`, `RestartSec=5`) is active
/// for a moment at the start of every round.
const STAY: Duration = Duration::from_secs(30);

/// How often the unit is looked at during [`STAY`].
const STAY_LOOK: Duration = Duration::from_secs(1);

/// The most a look at the package's `kbf` may take.
const PARSE_TIMEOUT: Duration = Duration::from_secs(60);

/// The flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub format: Format,
    pub channel: Channel,
    pub unit: Unit,
    pub dir: PathBuf,
    pub apply: bool,
}

impl Config {
    /// Read the flags (after `--migrate-to-package`).
    ///
    /// # Errors
    /// What is wrong with them, in a line.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let (mut layout, mut channel, mut unit, mut dir) = (None, None, None, None);
        let mut apply = false;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            if name == "--apply" && inline.is_none() {
                apply = true;
                continue;
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))?,
            };
            let slot = match name {
                "--layout" => &mut layout,
                "--channel" => &mut channel,
                "--unit" => &mut unit,
                "--dir" => &mut dir,
                _ => return Err(format!("unknown argument {name}")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("{name} is given twice"));
            }
        }
        let format = match layout.as_deref() {
            Some("deb") => Format::Deb,
            Some("rpm") => Format::Rpm,
            _ => return Err("--layout is required and is deb or rpm".to_owned()),
        };
        let channel = channel.ok_or("--channel is required")?;
        let unit = unit.ok_or("--unit is required")?;
        let dir = PathBuf::from(dir.ok_or("--dir is required")?);
        if !dir.is_absolute() {
            return Err("--dir must be an absolute path".to_owned());
        }
        Ok(Self {
            format,
            channel: Channel::new(&channel)
                .map_err(|_| format!("--channel {channel:?} is not a channel"))?,
            unit: Unit::service(&unit).map_err(|_| format!("--unit {unit:?} is not a kbf unit"))?,
            dir,
            apply,
        })
    }
}

/// Where the things the migration touches outside the package manager are.
#[derive(Debug, Clone)]
pub struct Place {
    /// `/etc/systemd/system`.
    pub systemd_dir: PathBuf,
    /// `/proc`.
    pub proc_dir: PathBuf,
    /// `/usr/bin/kbf`, the packaged binary the unit must come to run.
    pub kbf: PathBuf,
    /// `/`: the directories the node's flags name are looked at under it.
    pub root: PathBuf,
}

impl Place {
    pub fn production() -> Self {
        Self {
            systemd_dir: PathBuf::from("/etc/systemd/system"),
            proc_dir: PathBuf::from("/proc"),
            kbf: PathBuf::from(KBF),
            root: PathBuf::from("/"),
        }
    }
}

/// One step of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Install { file: PathBuf, dirs: Vec<Dir> },
    DropIn { path: PathBuf, text: String },
    MoveAside { from: PathBuf, to: PathBuf },
    Reload,
    Restart,
    Check { commit: String, dirs: Vec<Dir> },
    RemoveOldLayout(PathBuf),
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Install { file, dirs } => {
                write!(f, "install the verified package {}", file.display())?;
                if !dirs.is_empty() {
                    write!(
                        f,
                        ", then put back any of {} the install changed",
                        Dir::list(dirs)
                    )?;
                }
                Ok(())
            }
            Self::DropIn { path, text } => {
                write!(f, "write {}:", path.display())?;
                for line in text.lines() {
                    write!(f, "\n    | {line}")?;
                }
                Ok(())
            }
            Self::MoveAside { from, to } => {
                write!(f, "rename {} to {}", from.display(), to.display())
            }
            Self::Reload => f.write_str("systemctl daemon-reload"),
            Self::Restart => f.write_str("restart the unit and wait until it is active"),
            Self::Check { commit, dirs } => {
                write!(
                    f,
                    "check that the unit's main process runs /usr/bin/kbf and that it reports commit {}",
                    &commit[..12]
                )?;
                if !dirs.is_empty() {
                    write!(f, ", and that {} are as they were", Dir::list(dirs))?;
                }
                Ok(())
            }
            Self::RemoveOldLayout(dir) => write!(
                f,
                "remove the old layout {} (only after the check above)",
                dir.display()
            ),
        }
    }
}

/// A directory of the node's data that the migration does not own, as it was before anything
/// changed: its mode and owner are put back if the install or the new unit changes them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dir {
    /// Where it is on this host.
    path: PathBuf,
    /// Where the node's flags name it.
    shown: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Dir {
    /// `/var/lib/kbf (mode 0755, owner 995:995)`, for each.
    fn list(dirs: &[Self]) -> String {
        dirs.iter()
            .map(|d| {
                format!(
                    "{} (mode {:04o}, owner {}:{})",
                    d.shown.display(),
                    d.mode,
                    d.uid,
                    d.gid
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Put the mode and owner back if they are not what they were.
    fn restore(&self) -> Result<(), String> {
        if self.unchanged() {
            return Ok(());
        }
        let at = |e: std::io::Error| format!("{}: {e}", self.shown.display());
        std::os::unix::fs::chown(&self.path, Some(self.uid), Some(self.gid)).map_err(at)?;
        // After the owner: a change of owner can clear mode bits.
        fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode)).map_err(at)
    }

    /// Whether it has the mode and owner it had.
    fn unchanged(&self) -> bool {
        fs::metadata(&self.path)
            .is_ok_and(|m| (m.mode() & 0o7777, m.uid(), m.gid()) == (self.mode, self.uid, self.gid))
    }
}

/// Put every directory back as it was; every one that could not be is said.
fn restore_all(dirs: &[Dir]) -> Result<(), String> {
    let problems: Vec<String> = dirs.iter().filter_map(|d| d.restore().err()).collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// The value of the flag `name` in `flags`, as `--name=value` or `--name value`; the last one
/// wins.
fn flag_value<'a>(flags: &'a [String], name: &str) -> Option<&'a str> {
    let mut found = None;
    let mut words = flags.iter();
    while let Some(word) = words.next() {
        match word.strip_prefix(name) {
            Some("") => found = words.next().map(String::as_str),
            Some(rest) if rest.starts_with('=') => found = rest.strip_prefix('='),
            _ => {}
        }
    }
    found
}

/// The directory a daemon of `mode` keeps its data in: `--data-dir`, or the default. None for a
/// mode that runs no containers.
fn data_dir(flags: &[String], mode: &str) -> Option<PathBuf> {
    RUNS_CONTAINERS
        .contains(&mode)
        .then(|| PathBuf::from(flag_value(flags, "--data-dir").unwrap_or(DEFAULT_DATA_DIR)))
}

/// The directories the node's flags name for its data: the data directory, the metadata
/// directory and a cold store on a local disk. The node's, not the package's.
fn node_dirs(flags: &[String], mode: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let named = [
        data_dir(flags, mode),
        flag_value(flags, "--meta-dir").map(PathBuf::from),
        flag_value(flags, "--cold-store")
            .and_then(|url| url.strip_prefix("file://"))
            .map(PathBuf::from),
    ];
    for dir in named.into_iter().flatten() {
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Where `path` (as the node names it) is on this host.
fn under(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

/// A refusal: nothing was changed.
struct Refused(String);

fn refused<T>(why: impl Into<String>) -> Result<T, Refused> {
    Err(Refused(why.into()))
}

impl From<Fault> for Refused {
    fn from(fault: Fault) -> Self {
        Self(fault.detail)
    }
}

/// The drop-in for the unit `unit_name`, which runs `old`'s flags as the mode `mode`.
///
/// # Errors
/// The flags are ones the packaged unit cannot give `kbf` ([`unit::HandPlaced::flags`]).
fn drop_in(old: &unit::HandPlaced, mode: &str, unit_name: &str) -> Result<String, String> {
    let flags = old.flags(mode, unit_name)?.join(" ");
    let mut text = String::from(
        "# Written by kbf-host-install --migrate-to-package: the flags this node's hand-placed\n\
         # unit gave kbf, now on the kbf package's unit and its /usr/bin/kbf.\n\
         [Unit]\n\
         ConditionPathExists=\n\n\
         [Service]\n",
    );
    text.push_str(&format!("User={}\n", old.user.as_deref().unwrap_or("root")));
    if let Some(group) = &old.group {
        text.push_str(&format!("Group={group}\n"));
    }
    for (directive, value) in &old.carried {
        text.push_str(&format!("{directive}={value}\n"));
    }
    // The packaged unit's StateDirectory= would make systemd chown and chmod /var/lib/kbf (to
    // 0750) at every start; the node's data directories are the node's, in the mode it set.
    text.push_str("StateDirectory=\n");
    text.push_str(&format!(
        "ExecStart=\nExecStart={KBF} {flags}\nExecStartPre=\n\
         ExecStartPre=+{KBF} boot-check --mode={mode}\nExecStartPre={KBF} {flags} --check\n"
    ));
    Ok(text)
}

fn systemctl(args: &[&str]) -> Cmd {
    Cmd {
        program: SYSTEMCTL.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout: Some(std::time::Duration::from_secs(120)),
        ..Cmd::default()
    }
}

/// What the plan is made of, and the copies that must outlive it: the package is installed
/// from them.
struct Planned {
    steps: Vec<Step>,
    /// What was checked, in a line each.
    notes: Vec<String>,
    /// The node's directories, as they are before anything changes.
    dirs: Vec<Dir>,
    _copies: Copies,
}

/// Everything that is checked before anything changes, and the plan.
fn plan_of(config: &Config, host: &PackageHost, place: &Place) -> Result<Planned, Refused> {
    let layout = &host.layout;
    let releases = layout.join(&layout.releases());
    // 1. The unit is the unit of a daemon mode.
    let mode = config
        .unit
        .as_str()
        .strip_prefix("kbf-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .filter(|mode| MODES.contains(mode))
        .ok_or_else(|| {
            Refused(format!(
                "{} is not the unit of a daemon mode ({})",
                config.unit.as_str(),
                MODES.join(", ")
            ))
        })?;
    // 2. The host runs a hand-placed release, and none is on trial.
    let state =
        state::read_unlocked(layout).map_err(|e| Refused(format!("the installer's state: {e}")))?;
    let Some(state) = state else {
        return refused("this host has no installer state: it is not on the hand-placed layout");
    };
    let Some(current) = state.current else {
        return refused("the installer's state names no release that runs");
    };
    if state.pending_confirm.is_some() {
        return refused("a release is on trial: confirm or revert it first");
    }
    let commit = current.to_string();

    // 3. The unit is one the migration carries over: root's alone to write, a plain file.
    let unit_file = place.systemd_dir.join(config.unit.as_str());
    let text = host
        .host
        .trust
        .read(&unit_file)
        .map_err(|e| Refused(format!("{}: {e}", unit_file.display())))?;
    let old = unit::parse(&text, &releases.display().to_string()).map_err(Refused)?;
    // The flags as the packaged unit gives them (the mode word of the old spelling is gone).
    let flags = old.flags(mode, config.unit.as_str()).map_err(Refused)?;
    let text_of_drop_in = drop_in(&old, mode, config.unit.as_str()).map_err(Refused)?;
    let dirs = record_dirs(place, &flags, mode)?;
    let drop_in_dir = place
        .systemd_dir
        .join(format!("{}.d", config.unit.as_str()));
    let aside = place
        .systemd_dir
        .join(format!("{}{ASIDE}", config.unit.as_str()));
    for taken in [drop_in_dir.join(DROP_IN), aside.clone()] {
        if fs::symlink_metadata(&taken).is_ok() {
            return refused(format!("{} is there already", taken.display()));
        }
    }

    // 3. The package for that very commit passes every check.
    let (settings, _token, origin) = host.prepare(&config.channel)?;
    let who = Identity {
        uid: settings.run_as.uid,
        gid: settings.run_as.gid,
    };
    let platform = format!("linux-{}", settings.arch);
    let file_name = release::named_asset_on(
        origin.prefix(),
        &platform,
        config.format.asset().extension(),
        &commit,
    );
    let work = host.layout.join(&host.layout.work_dir());
    let copies = host.copy_in(
        (&work, &commit),
        who,
        [
            &config.dir.join(&file_name),
            &config.dir.join(release::bundle_of(&file_name)),
            &config.dir.join(release::SUMS_NAME),
        ],
        &file_name,
    )?;
    let meta = host.check_copies(&copies, &current, &settings)?;
    if commit_prefix(&meta.version) != Some(&commit[..12]) {
        return refused(format!(
            "the package is version {}, which is not the commit {} the host runs",
            meta.version,
            &commit[..12]
        ));
    }

    // 4. The package's own kbf accepts those flags, and its unit leaves the directories be.
    let notes = check_package(host, config, place, &copies, who, &flags, &dirs)?;

    let steps = vec![
        Step::Install {
            file: copies.package.clone(),
            dirs: dirs.clone(),
        },
        Step::DropIn {
            path: drop_in_dir.join(DROP_IN),
            text: text_of_drop_in,
        },
        Step::MoveAside {
            from: unit_file,
            to: aside,
        },
        Step::Reload,
        Step::Restart,
        Step::Check {
            commit,
            dirs: dirs.clone(),
        },
        Step::RemoveOldLayout(releases),
    ];
    Ok(Planned {
        steps,
        notes,
        dirs,
        _copies: copies,
    })
}

/// The node's directories as they are now, and a refusal when the container runtime cannot
/// reach the data directory: it needs every directory from `/` down to it searchable by others,
/// and the migration keeps the modes it finds and chooses none.
fn record_dirs(place: &Place, flags: &[String], mode: &str) -> Result<Vec<Dir>, Refused> {
    let mut dirs = Vec::new();
    for shown in node_dirs(flags, mode) {
        let path = under(&place.root, &shown);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => dirs.push(Dir {
                path,
                shown,
                mode: meta.mode() & 0o7777,
                uid: meta.uid(),
                gid: meta.gid(),
            }),
            Ok(_) => return refused(format!("{} is not a directory", shown.display())),
            // Not there (or not to be looked into): nothing of the node's to keep.
            Err(_) => {}
        }
    }
    if let Some(data) = data_dir(flags, mode) {
        let start = under(&place.root, &data);
        for ancestor in start.ancestors().take_while(|a| *a != place.root) {
            if let Ok(meta) = fs::metadata(ancestor)
                && meta.mode() & 0o001 == 0
            {
                let shown =
                    Path::new("/").join(ancestor.strip_prefix(&place.root).unwrap_or(ancestor));
                return refused(format!(
                    "{} is not searchable by others (mode {:04o}): the container runtime of a {mode} \
                     node reaches {} through it. Run chmod o+x {}; the move keeps the modes it finds",
                    shown.display(),
                    meta.mode() & 0o7777,
                    data.display(),
                    shown.display()
                ));
            }
        }
    }
    Ok(dirs)
}

/// What the package's own files say, with nothing started: unpack the package (a deb) and run
/// its `kbf` on the flags the drop-in gives it, with `--help` last, so that it reads every flag
/// and starts nothing; and read its unit for the directories it would change.
///
/// An rpm is not unpacked here: its flags have the same rule as the parser's (the mode is named
/// by `--mode=` alone), and the note says it was not run.
fn check_package(
    host: &PackageHost,
    config: &Config,
    place: &Place,
    copies: &Copies,
    who: Identity,
    flags: &[String],
    dirs: &[Dir],
) -> Result<Vec<String>, Refused> {
    let mut notes = Vec::new();
    for dir in dirs {
        notes.push(format!(
            "{} keeps mode {:04o} and owner {}:{}",
            dir.shown.display(),
            dir.mode,
            dir.uid,
            dir.gid
        ));
    }
    let unpacked = copies.dir().join("unpacked");
    let Some(unpack) = host.format.unpack(&copies.package, &unpacked) else {
        notes.push(
            "the package is not unpacked here, so its kbf was not run on the flags and its unit \
             was not read: the flags follow the parser's rule (the mode is named by --mode= alone)"
                .to_owned(),
        );
        return Ok(notes);
    };
    let made = |e: std::io::Error| Refused(format!("the package check: {e}"));
    fs::create_dir(&unpacked)
        .and_then(|()| fs::set_permissions(&unpacked, fs::Permissions::from_mode(0o755)))
        .map_err(made)?;
    ran(&*host.runner, &unpack, "unpacking the package").map_err(Refused)?;
    // An empty kbf.toml: the flags are read alone, whatever file the host has.
    let empty = copies.dir().join("empty.toml");
    fs::write(&empty, b"")
        .and_then(|()| fs::set_permissions(&empty, fs::Permissions::from_mode(0o644)))
        .map_err(made)?;
    let mut args = flags.to_vec();
    args.push(format!("--config={}", empty.display()));
    args.push("--help".to_owned());
    let parse = Cmd {
        program: unpacked.join("usr/bin/kbf").display().to_string(),
        args,
        timeout: Some(PARSE_TIMEOUT),
        run_as: Some(who),
        stdout_max: Some(4096),
        ..Cmd::default()
    };
    ran(
        &*host.runner,
        &parse,
        "the package's kbf does not accept the flags the drop-in gives it",
    )
    .map_err(Refused)?;
    notes.push(
        "the package's kbf reads the flags of the drop-in (run with --help last: nothing starts)"
            .to_owned(),
    );
    // What the packaged unit would do to the directories, if the drop-in did not clear it.
    let unit_file = unpacked
        .join("usr/lib/systemd/system")
        .join(config.unit.as_str());
    notes.extend(
        fs::read_to_string(unit_file)
            .map(|text| state_dir_changes(&text, place))
            .unwrap_or_default(),
    );
    Ok(notes)
}

/// The `StateDirectory=` the packaged unit names, and what it would do to each that exists:
/// systemd sets the mode (`StateDirectoryMode=`, 0755 when it is not given) at every start.
fn state_dir_changes(unit_text: &str, place: &Place) -> Vec<String> {
    let (mut names, mut want): (Vec<String>, u32) = (Vec::new(), 0o755);
    let mut in_service = false;
    for line in unit_text.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[') {
            in_service = section.trim_end_matches(']') == "Service";
        } else if in_service && let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "StateDirectory" if value.trim().is_empty() => names.clear(),
                "StateDirectory" => names.extend(value.split_whitespace().map(str::to_owned)),
                "StateDirectoryMode" => {
                    want = u32::from_str_radix(value.trim(), 8).unwrap_or(want);
                }
                _ => {}
            }
        }
    }
    names
        .iter()
        .filter_map(|name| {
            let shown = Path::new("/var/lib").join(name);
            let now = fs::metadata(under(&place.root, &shown)).ok()?.mode() & 0o7777;
            (now != want).then(|| {
                format!(
                    "the package's unit would set {} from mode {now:04o} to {want:04o} \
                     (StateDirectory=); the drop-in clears StateDirectory=, so it stays",
                    shown.display()
                )
            })
        })
        .collect()
}

/// The commit `kbf --version --json` reports.
fn reported_commit(runner: &dyn Runner) -> Option<String> {
    let exec = runner
        .run(&plan::running_build())
        .ok()
        .filter(crate::runner::Exec::success)?;
    let build: Value = serde_json::from_str(&exec.stdout_text()).ok()?;
    ["commit_full", "commit"]
        .iter()
        .filter_map(|key| build.get(key)?.as_str())
        .find(|commit| (1..=64).contains(&commit.len()))
        .map(str::to_owned)
}

/// Run `cmd`; why not, in a line that starts with `what`, when it cannot be run or does not
/// exit 0.
fn ran(runner: &dyn Runner, cmd: &Cmd, what: &str) -> Result<crate::runner::Exec, String> {
    match runner.run(cmd) {
        Ok(exec) if exec.success() => Ok(exec),
        Ok(exec) => Err(format!(
            "{what}: exit status {:?}: {}",
            exec.code,
            exec.stderr_text().trim()
        )),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

/// Carry out one step.
fn apply(step: &Step, host: &PackageHost, config: &Config, place: &Place) -> Result<(), String> {
    let runner = &*host.runner;
    let ran = |cmd: &Cmd, what: &str| ran(runner, cmd, what);
    match step {
        Step::Install { file, dirs } => {
            ran(&config.format.install(file), "installing the package")?;
            restore_all(dirs)
        }
        Step::DropIn { path, text } => {
            let dir = path.parent().ok_or("the drop-in has no directory")?;
            fs::create_dir_all(dir)
                .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o755)))
                .and_then(|()| fs::write(path, text))
                .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o644)))
                .map_err(|e| format!("{}: {e}", path.display()))
        }
        Step::MoveAside { from, to } => {
            fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))
        }
        Step::Reload => ran(&systemctl(&["daemon-reload"]), "systemctl daemon-reload").map(drop),
        Step::Restart => restart_and_wait(runner, &config.unit, host.restart_wait, &host.pace),
        Step::Check { commit, dirs } => check(host, config, place, commit, dirs),
        Step::RemoveOldLayout(dir) => {
            fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
        }
    }
}

/// The unit's main process runs the packaged binary, and the binary is the commit the host ran.
fn check(
    host: &PackageHost,
    config: &Config,
    place: &Place,
    commit: &str,
    dirs: &[Dir],
) -> Result<(), String> {
    let runner = &*host.runner;
    if let Some(changed) = dirs.iter().find(|d| !d.unchanged()) {
        return Err(format!(
            "the start changed {}, which the node keeps as it was",
            changed.shown.display()
        ));
    }
    let shown = runner
        .run(&systemctl(&[
            "show",
            "--property=MainPID",
            "--value",
            "--",
            config.unit.as_str(),
        ]))
        .map_err(|e| format!("systemctl show: {e}"))?;
    let pid: u32 = shown
        .stdout_text()
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or("the unit has no main process")?;
    let exe = fs::read_link(place.proc_dir.join(pid.to_string()).join("exe"))
        .map_err(|e| format!("the main process {pid}: {e}"))?;
    if exe != place.kbf {
        return Err(format!(
            "the main process runs {}, not {}",
            exe.display(),
            place.kbf.display()
        ));
    }
    match reported_commit(runner) {
        Some(reported) if commit.starts_with(&reported) => Ok(()),
        Some(reported) => Err(format!(
            "{} reports commit {reported}, not {}",
            place.kbf.display(),
            &commit[..12]
        )),
        None => Err(format!("{} reports no commit", place.kbf.display())),
    }
}

/// Restart `unit` and wait until it is active ([`restart_and_wait`]), then look at it every
/// interval for [`STAY`]: a unit that is active for a moment and then fails, and is restarted,
/// is not running. The error names the state it was seen in.
fn restart_and_stay(
    runner: &dyn Runner,
    unit: &Unit,
    wait: Wait,
    pace: &Pace,
) -> Result<(), String> {
    restart_and_wait(runner, unit, wait, pace)?;
    let end = (pace.now)() + STAY;
    while (pace.now)() < end {
        (pace.sleep)(STAY_LOOK);
        let state = match runner.run(&plan::unit_active(unit)) {
            Ok(exec) if exec.success() => continue,
            Ok(exec) => exec.stdout_text().trim().to_owned(),
            Err(e) => e.to_string(),
        };
        return Err(format!(
            "{} was active after the restart but did not stay active: it is {}",
            unit.as_str(),
            if state.is_empty() {
                "not active"
            } else {
                &state
            }
        ));
    }
    Ok(())
}

/// Put the hand-placed unit back after a failure: the drop-in goes, the unit is renamed back,
/// systemd rereads, the node's directories get their mode and owner back, and the unit restarts
/// and is watched for a while. Best effort, and says what it could not do.
fn put_back(
    host: &PackageHost,
    config: &Config,
    place: &Place,
    dirs: &[Dir],
) -> Result<(), String> {
    let unit_file = place.systemd_dir.join(config.unit.as_str());
    let aside = place
        .systemd_dir
        .join(format!("{}{ASIDE}", config.unit.as_str()));
    let drop_in_dir = place
        .systemd_dir
        .join(format!("{}.d", config.unit.as_str()));
    let mut problems = Vec::new();
    let mut note = |step: &str, result: std::io::Result<()>| match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            problems.push(format!("{step}: {e}"));
        }
        _ => {}
    };
    note(
        "removing the drop-in",
        fs::remove_file(drop_in_dir.join(DROP_IN)),
    );
    note(
        "removing the drop-in directory",
        fs::remove_dir(&drop_in_dir),
    );
    if fs::symlink_metadata(&aside).is_ok() {
        note("renaming the unit back", fs::rename(&aside, &unit_file));
    }
    let reloaded = host.runner.run(&systemctl(&["daemon-reload"]));
    if !matches!(&reloaded, Ok(exec) if exec.success()) {
        problems.push("systemctl daemon-reload did not work".to_owned());
    }
    // Before the restart: the hand-placed unit's container runtime needs the modes it had.
    if let Err(why) = restore_all(dirs) {
        problems.push(format!("putting the directories back: {why}"));
    }
    if let Err(why) = restart_and_stay(&*host.runner, &config.unit, host.restart_wait, &host.pace) {
        problems.push(why);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Run the migration against `host`: print the plan, and carry it out with `--apply`. The
/// exit status: 0 done or planned, 1 a step failed, 2 refused.
pub fn run_with(
    config: &Config,
    host: &PackageHost,
    place: &Place,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let planned = match plan_of(config, host, place) {
        Ok(planned) => planned,
        Err(Refused(why)) => {
            let _ = writeln!(err, "kbf-host-install: refused: {why}");
            return 2;
        }
    };
    for note in &planned.notes {
        let _ = writeln!(out, "checked: {note}");
    }
    if !config.apply {
        for step in &planned.steps {
            let _ = writeln!(out, "DRY-RUN: {step}");
        }
        let _ = writeln!(out, "dry run: nothing changed. Re-run with --apply.");
        return 0;
    }
    for (index, step) in planned.steps.iter().enumerate() {
        let _ = writeln!(out, "+ {step}");
        if let Err(why) = apply(step, host, config, place) {
            let _ = writeln!(err, "kbf-host-install: step {} failed: {why}", index + 1);
            // Up to the removal, the old layout is whole: the old unit comes back. (A removal
            // that fails leaves a node that runs the package and a directory to delete.)
            if index >= 1 && !matches!(step, Step::RemoveOldLayout(_)) {
                match put_back(host, config, place, &planned.dirs) {
                    Ok(()) => {
                        let _ = writeln!(
                            err,
                            "kbf-host-install: the hand-placed unit is back and running"
                        );
                    }
                    Err(problems) => {
                        let _ =
                            writeln!(err, "kbf-host-install: putting it back failed: {problems}");
                    }
                }
            }
            return 1;
        }
    }
    let _ = writeln!(
        out,
        "migrated: {} runs the kbf package",
        config.unit.as_str()
    );
    0
}

/// The program: the flags, then the migration of this host.
pub fn main(args: &[String]) -> ExitCode {
    let config = match Config::parse(args) {
        Ok(config) => config,
        Err(why) => {
            eprintln!("kbf-host-install: {why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let host = crate::local::package::production(
        Vec::new(),
        config.format,
        Path::new("/var/lib/kbf-host-install"),
    );
    ExitCode::from(run_with(
        &config,
        &host,
        &Place::production(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
