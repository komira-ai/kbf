//! A host the Mac kit installed (`deploy/kbf-mac/kbf_av2_mac.sh`), read as an old host.
//!
//! The kit keeps no `state.json`, `helper.json` or lock file: a Mac has no `/run`, and what
//! is current is what the link `/usr/local/kbf/current` names and what is previous is what
//! `previous` names (the kit reads them with `readlink`). So the links are the truth here,
//! and what runs is the LaunchDaemon `com.komira.kbf-daemon`. A host is a kit host when the
//! old state is absent and its tree is a whole kit tree on macOS: `releases/`, a `current`
//! link and that plist. Two markers say the kit is there: its settings file
//! `etc/av2_mac.conf` (the kit dies without it on every verb) and the plist whose
//! `ProgramArguments` run `/usr/local/kbf/current/...`. With one and not the other the host
//! is half removed or not the kit's, and the migration refuses and says which is missing
//! rather than guess; with neither it is not a kit host. Which of the kit's two argument forms the job is in is read from
//! the plist and is not a difference migrate acts on: the plist is repointed whole-path as
//! for any old host, and every other byte stays.

use std::path::Path;

use super::error::MigrateError;
use super::old::{Old, OldState, OldTree};
use super::roots::Roots;
use super::tree::{self, Rules};
use crate::helper::memory::Memory;
use crate::layout::{self, Layout, Platform};

/// The kit's job, by file name, in the LaunchDaemons directory.
pub const PLIST: &str = "com.komira.kbf-daemon.plist";
/// Where launchd jobs are on a Mac; on Linux this is where a kit's job must not be.
const MAC_UNIT_DIR: &str = "/Library/LaunchDaemons";
/// What the job runs, as the kit writes it.
const PROGRAM: &str = "/usr/local/kbf/current/kbf";
/// A file only the kit writes into the tree: the kit's own settings, which `mac_join` writes
/// and the kit cannot run without. It is one of the kit's two markers; the job is the other.
const KIT_MARKER: &str = "etc/av2_mac.conf";
/// Where the kit's job runs from: a plist pointing into it is the kit's job.
const CURRENT_DIR: &str = "/usr/local/kbf/current/";
const PLIST_CAP: u64 = 64 * 1024;

/// How the kit's job passes its settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlistForm {
    /// `kbf --mode=daemon --config <file>`: settings are keys of the config file.
    ModeFlag,
    /// `kbf daemon <flags>`: settings are flags.
    DaemonWord,
}

impl PlistForm {
    pub fn describe(self) -> &'static str {
        match self {
            PlistForm::ModeFlag => "the --mode=daemon form",
            PlistForm::DaemonWord => "the daemon-word form",
        }
    }
}

/// The `<string>` values of the plist's `ProgramArguments`, in order; none when the plist
/// has no such array.
fn program_arguments(plist: &str) -> Vec<String> {
    let array = plist
        .split_once("<key>ProgramArguments</key>")
        .and_then(|(_, rest)| rest.split_once("<array>"))
        .and_then(|(_, rest)| rest.split_once("</array>"));
    let Some((body, _)) = array else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>"))
        .map(str::to_owned)
        .collect()
}

/// The form of the job, or why it is none of the kit's two. The job runs `PROGRAM`, or, when
/// `repointed` names the installer's link, that link as well: a run of this migration that
/// died after it repointed the plist left it so (`units::rewrite`), and its journal says it.
fn form(plist: &str, repointed: Option<&str>) -> Result<PlistForm, MigrateError> {
    let args = program_arguments(plist);
    let is_program = |p: &str| p == PROGRAM || repointed == Some(p);
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [p, "--mode=daemon", "--config", _file] if is_program(p) => Ok(PlistForm::ModeFlag),
        [p, "daemon", ..] if is_program(p) => Ok(PlistForm::DaemonWord),
        _ => Err(MigrateError::UnknownPlist(format!(
            "{PLIST} runs {args:?}, not `{PROGRAM} --mode=daemon --config <file>` or `{PROGRAM} daemon ...`"
        ))),
    }
}

fn present(path: &Path) -> Result<bool, MigrateError> {
    let found = tree::absent(path.symlink_metadata());
    Ok(found
        .map_err(MigrateError::io(path.display().to_string()))?
        .is_some())
}

/// The kit's job file, read; `None` when it or the LaunchDaemons directory is not there.
fn read_job(
    layout: &Layout,
    roots: &Roots,
    rules: &Rules,
) -> Result<Option<Vec<u8>>, MigrateError> {
    let units = tree::absent(tree::open_root(&layout.join(&roots.unit_dir)))
        .map_err(MigrateError::io(roots.unit_dir.to_string()))?;
    match &units {
        Some(dir) => Ok(tree::read_file(rules, dir, PLIST, PLIST_CAP)?),
        None => Ok(None),
    }
}

/// Whether the job's program is one in the kit's `current/` (or `repointed`, the link a
/// migration that died left it at).
fn runs_current(plist: &str, repointed: Option<&str>) -> bool {
    program_arguments(plist)
        .first()
        .is_some_and(|p| p.starts_with(CURRENT_DIR) || repointed == Some(p.as_str()))
}

/// Read a host with no old state: `None` when it is not a kit host. `resume` is set when the
/// migration's journal says a kit migration began here: the plist may then already run the
/// installer's `current/kbf`, which is what a run that died after repointing it left. Without
/// that journal the repointed form is refused as any other.
pub fn read(
    layout: &Layout,
    roots: &Roots,
    rules: &Rules,
    resume: bool,
) -> Result<Option<Old>, MigrateError> {
    let path = layout.join(&roots.old_tree);
    let what = roots.old_tree.to_string();
    let Some(tree) = tree::absent(tree::open_root(&path)).map_err(MigrateError::io(&what))? else {
        return Ok(None);
    };
    let has = |name: &str| present(&path.join(name));
    let (releases, links) = (has("releases")?, has("current")? || has("previous")?);
    if !releases && !links {
        return Ok(None);
    }
    let plist_path = layout.join(&layout::path(MAC_UNIT_DIR)).join(PLIST);
    if roots.platform == Platform::Linux {
        return if present(&plist_path)? {
            Err(MigrateError::KitOnLinux(plist_path.display().to_string()))
        } else {
            Ok(None)
        };
    }
    drop(tree);
    let conf = present(&layout.join(&roots.old_tree).join(KIT_MARKER))?;
    let repointed = resume.then(|| format!("{}/kbf", layout.current()));
    let job = read_job(layout, roots, rules)?;
    let marker = job
        .as_deref()
        .is_some_and(|b| runs_current(&String::from_utf8_lossy(b), repointed.as_deref()));
    let unit_dir = roots.unit_dir.to_string();
    let bytes = match (conf, job, marker) {
        // Neither marker: not the kit's tree (a plist that runs something else is not the
        // kit's job, and the tree alone is no kit).
        (false, _, false) => return Ok(None),
        (false, _, true) => {
            return Err(MigrateError::HalfKit(format!(
                "{what}/{KIT_MARKER} is missing and {PLIST} running {CURRENT_DIR}... is in \
                 {unit_dir}: a kit host has both, and the migration will not guess which this is"
            )));
        }
        (true, None, _) => {
            return Err(MigrateError::HalfKit(format!(
                "{what}/{KIT_MARKER} is there and {PLIST} is not in {unit_dir}: a kit host has \
                 both, and the migration will not guess which this is"
            )));
        }
        (true, Some(bytes), _) => bytes,
    };
    if !releases {
        return Err(MigrateError::HalfKit(format!(
            "{what} has links and no releases/"
        )));
    }
    let form = form(&String::from_utf8_lossy(&bytes), repointed.as_deref())?;
    let old_tree = OldTree::open(layout, roots)?;
    let current = old_tree.linked("current")?;
    if current.is_none() {
        return Err(MigrateError::HalfKit(format!("{what} has no current link")));
    }
    let previous = old_tree.linked("previous")?;
    Ok(Some(Old {
        state: OldState::from_links(current, previous),
        memory: Memory::default(),
        kit: Some(form),
    }))
}

/// Old state beside a kit tree: two managers' state, and the migration does not choose.
/// The kit's settings file and its job are both there for that; the settings file without
/// the job is a half-removed kit beside state and is refused as such. The job without the
/// settings file is the old tools' own job (they ran `current/kbf` too), and is an old host.
pub fn refuse_beside_state(
    layout: &Layout,
    roots: &Roots,
    rules: &Rules,
) -> Result<(), MigrateError> {
    let marker = layout.join(&roots.old_tree).join(KIT_MARKER);
    if roots.platform != Platform::MacOs || !present(&marker)? {
        return Ok(());
    }
    let job = read_job(layout, roots, rules)?;
    if !job.is_some_and(|b| runs_current(&String::from_utf8_lossy(&b), None)) {
        return Err(MigrateError::HalfKit(format!(
            "{} holds the old tools' state, {} is the kit's, and {PLIST} running {CURRENT_DIR}... \
             is missing from {}: the migration will not guess which manager this host has",
            roots.old_state,
            marker.display(),
            roots.unit_dir
        )));
    }
    Err(MigrateError::Conflict(format!(
        "{} holds the old tools' state and {} is the kit's: the links are the kit's truth and \
         state.json is the old tools', and the migration will not choose between them",
        roots.old_state,
        marker.display()
    )))
}
