//! A host the Mac kit installed (`deploy/kbf-mac/kbf_av2_mac.sh`), read as an old host.
//!
//! The kit keeps no `state.json`, `helper.json` or lock file: a Mac has no `/run`, and what
//! is current is what the link `/usr/local/kbf/current` names and what is previous is what
//! `previous` names (the kit reads them with `readlink`). So the links are the truth here,
//! and what runs is the LaunchDaemon `com.komira.kbf-daemon`. A host is a kit host when the
//! old state is absent and its tree is a whole kit tree on macOS: `releases/`, a `current`
//! link and that plist. Which of the kit's two argument forms the job is in is read from
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
/// A file only the kit writes into the tree: the kit's own settings. State beside it is
/// two managers' state.
const KIT_MARKER: &str = "etc/av2_mac.conf";
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
    if !releases {
        return Err(MigrateError::HalfKit(format!(
            "{what} has links and no releases/"
        )));
    }
    let units = tree::absent(tree::open_root(&layout.join(&roots.unit_dir)))
        .map_err(MigrateError::io(roots.unit_dir.to_string()))?;
    let bytes = match &units {
        Some(dir) => tree::read_file(rules, dir, PLIST, PLIST_CAP)?,
        None => None,
    };
    let Some(bytes) = bytes else {
        return Err(MigrateError::HalfKit(format!(
            "{what} is there and {PLIST} is not in {}: a kit host has both",
            roots.unit_dir
        )));
    };
    let repointed = resume.then(|| format!("{}/kbf", layout.current()));
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
pub fn refuse_beside_state(layout: &Layout, roots: &Roots) -> Result<(), MigrateError> {
    let marker = layout.join(&roots.old_tree).join(KIT_MARKER);
    if roots.platform == Platform::MacOs && present(&marker)? {
        return Err(MigrateError::Conflict(format!(
            "{} holds the old tools' state and {} is the kit's: the links are the kit's truth and \
             state.json is the old tools', and the migration will not choose between them",
            roots.old_state,
            marker.display()
        )));
    }
    Ok(())
}
