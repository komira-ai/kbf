//! The generated build table (`buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/`),
//! fresh before the farm build, and the fix handed over when it is not.
//!
//! buildrules generates the table from the Cargo files (`Cargo.toml`, `Cargo.lock`,
//! `rust-toolchain.toml`) with `buildrules sync`; nobody edits it by hand. A pull request that
//! changes a Cargo file and not the table fails the build. A contributor need not have the
//! `buildrules` command line, so the job, which builds it from the pinned buildrules cell anyway,
//! makes the fix itself:
//!
//! 1. `buck2 build buildrules//cli:buildrules --show-full-output` builds the command line on the
//!    farm (it is a target of the cell `.buckconfig` pins) and names the file it was written to.
//! 2. `buildrules check`: status 0 is a fresh table, and the build goes on. Status 1 is a table
//!    that is stale or missing a generated file (its problems are what check printed); any other
//!    status (a usage error) is the error, with what check printed.
//! 3. For status 1: `buildrules sync --buck2 <buck2>`, with what it prints written to the log, then
//!    `buildrules check` again, which must now pass. A problem sync does not fix (a member with no
//!    `cargo_package` call) fails here, with check's words, and no patch is handed over.
//! 4. The patch: the files sync changed below the paths it writes ([`SYNCED`]), as
//!    `git diff --binary` of the working tree after its new files there are added to the index as
//!    intended (`git add -N`), so that `git apply` of it writes exactly what sync wrote. On a pull
//!    request the checkout is the merge of the branch into main, so the patch is made against that
//!    merge. It is written to [`Handoff::patch`]; the workflow uploads it as [`PATCH_ARTIFACT`].
//! 5. The job summary ([`Handoff::summary`], appended): the stale inputs, the changed files and
//!    the one command that downloads and applies the patch. The error says the same.
//!
//! The job does not push the patch to the pull request's branch: see docs/farm.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Error as FarmError, output, start_error, tee};

/// The target of the `buildrules` command line in the pinned buildrules cell.
pub const TOOL: &str = "buildrules//cli:buildrules";
/// The name of the artifact the workflow uploads the patch as.
pub const PATCH_ARTIFACT: &str = "buildrules-sync";
/// The patch's file name, in the artifact.
pub const PATCH_FILE: &str = "buildrules-sync.patch";
/// The start of every line of `buildrules check`'s problems, and of those that call the table stale.
const PREFIX: &str = "buildrules: ";
const STALE: &str = "STALE: ";
/// The fix `buildrules check` ends each of its lines with, which the job does for the reader.
const FIX: &str = ": run `buildrules sync` and commit the result";
/// The paths `buildrules sync` writes (`Cargo.lock` when its `cargo fetch` must change it, the root
/// `BUCK` only when there is none): the patch holds these and nothing else of the working tree.
pub const SYNCED: [&str; 6] = [
    "BUCK",
    "Cargo.lock",
    "buildrules.bzl",
    "buildrules.lock.bzl",
    "third_party/buildrules",
    "toolchains",
];
/// The `git diff` the patch and its file list are made with, whatever the reader's git config.
const DIFF: [&str; 9] = [
    "-c",
    "core.quotepath=off",
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// Where a stale table's fix goes, and the run that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The file the patch is written to; its directory is made when missing.
    pub patch: PathBuf,
    /// The job summary, appended to.
    pub summary: PathBuf,
    /// The repository of the run, `owner/name`.
    pub repo: String,
    /// The run's id.
    pub run_id: u64,
}

impl Handoff {
    /// The one command that downloads the patch of this run and applies it to a checkout of the
    /// branch, into a directory of `buck-out/` (which git ignores) of its own.
    pub fn apply_command(&self) -> String {
        let (run, repo) = (self.run_id, &self.repo);
        format!(
            "gh run download {run} -R {repo} -n {PATCH_ARTIFACT} -D buck-out/{PATCH_ARTIFACT}-{run} \
             && git apply buck-out/{PATCH_ARTIFACT}-{run}/{PATCH_FILE}"
        )
    }
}

/// Why the table is not fresh, or why that could not be settled.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The build of the `buildrules` command line failed.
    #[error("`buck2 build {TOOL}` failed ({status}): {stderr}")]
    Tool {
        /// How it ended.
        status: String,
        /// What buck2 printed on its standard error, trimmed.
        stderr: String,
    },
    /// The build of the command line named no file for it.
    #[error("`buck2 build {TOOL} --show-full-output` named no output of {TOOL}: {stdout:?}")]
    NoTool {
        /// What buck2 printed on its standard output.
        stdout: String,
    },
    /// `buildrules check` could not judge the table (a status other than 0 and 1).
    #[error("`buildrules check` could not judge the table ({status}): {stderr}")]
    Check {
        /// How it ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// The table is stale and `buildrules sync` failed; what it printed is in the log.
    #[error(
        "the generated build table is stale, and `buildrules sync` failed ({status}); its output is above"
    )]
    Sync {
        /// How it ended.
        status: String,
    },
    /// `buildrules sync` passed and `buildrules check` still fails.
    #[error("`buildrules sync` ran, and `buildrules check` still fails ({status}): {stderr}")]
    StillFails {
        /// How the second check ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// A git command of the patch failed.
    #[error("`git {args}` failed ({status}): {stderr}")]
    Git {
        /// Its arguments, joined by spaces.
        args: String,
        /// How it ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// The patch or the summary could not be written.
    #[error("cannot write {path:?}: {why}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// The table was stale: its fix is the patch of this run.
    #[error("{}", stale_message(inputs, files, apply))]
    Stale {
        /// The problems `buildrules check` found, without their marks and fix.
        inputs: Vec<String>,
        /// The files the patch changes.
        files: Vec<String>,
        /// The command that applies the patch.
        apply: String,
    },
}

/// The error's words for a stale table.
fn stale_message(inputs: &[String], files: &[String], apply: &str) -> String {
    format!(
        "the generated build table is stale: {}. buildrules.lock.bzl, buildrules.bzl and \
         third_party/buildrules/ are generated from the Cargo files by `buildrules sync`, which \
         this job ran at the pinned version; what it changed ({}) is the artifact \
         {PATCH_ARTIFACT} of this run, made against this run's merge of the branch into main. \
         Apply it in the branch's checkout, then commit and push (if it does not apply, merge \
         main first): {apply}",
        inputs.join("; "),
        files.join(", "),
    )
}

/// The job summary for a stale table, in Markdown.
pub fn summary_markdown(inputs: &[String], files: &[String], apply: &str) -> String {
    let list = |items: &[String], code: bool| -> String {
        items
            .iter()
            .map(|i| {
                if code {
                    format!("- `{i}`\n")
                } else {
                    format!("- {i}\n")
                }
            })
            .collect()
    };
    format!(
        "## The generated build table is stale\n\n\
         `buildrules check` found inputs of the generated build table changed since it was made:\n\n\
         {}\n\
         `buildrules.lock.bzl`, `buildrules.bzl` and `third_party/buildrules/` are generated from \
         the Cargo files (`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`) by `buildrules sync`, \
         and nobody edits them by hand. This job ran the pinned `buildrules sync` and uploaded \
         what it changed as the artifact `{PATCH_ARTIFACT}`, a patch of these files:\n\n\
         {}\n\
         Read it, then apply it in a checkout of the branch, commit and push. Nothing runs on \
         your machine but `gh` and `git`:\n\n\
         ```sh\n{apply}\n```\n\n\
         If `git apply` refuses it, main has changed the table too: merge main into the branch \
         and push, and the next run carries a new patch.\n",
        list(inputs, false),
        list(files, true),
    )
}

/// The file `buck2 build <TOOL> --show-full-output` printed for [`TOOL`], when it printed one.
pub fn tool_path(stdout: &str) -> Option<PathBuf> {
    stdout.lines().find_map(|line| {
        let (label, path) = line.split_once(' ')?;
        let path = path.trim();
        (label == TOOL && path.starts_with('/')).then(|| PathBuf::from(path))
    })
}

/// The problems the lines of `buildrules check` (its standard error) name, without the
/// `buildrules:` and `STALE:` marks and the fix each ends with.
pub fn stale_inputs(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .map(|line| line.strip_prefix(STALE).unwrap_or(line))
        .map(|line| line.strip_suffix(FIX).unwrap_or(line).to_owned())
        .collect()
}

/// Fails unless the generated build table is fresh, leaving its fix where `handoff` says when
/// it is stale.
pub(super) fn ensure_fresh(
    buck2: &Path,
    handoff: &Handoff,
    log: &mut dyn Write,
) -> Result<(), FarmError> {
    let built = output(buck2, &["build", TOOL, "--show-full-output"])?;
    if !built.status.success() {
        return Err(Error::Tool {
            status: built.status.to_string(),
            stderr: trimmed(&built.stderr),
        }
        .into());
    }
    let stdout = String::from_utf8_lossy(&built.stdout);
    let tool = tool_path(&stdout).ok_or_else(|| Error::NoTool {
        stdout: stdout.into_owned(),
    })?;
    let checked = output(&tool, &["check"])?;
    if checked.status.success() {
        return Ok(());
    }
    let stderr = trimmed(&checked.stderr);
    if checked.status.code() != Some(1) {
        return Err(Error::Check {
            status: checked.status.to_string(),
            stderr,
        }
        .into());
    }
    let inputs = stale_inputs(&stderr);
    sync(&tool, buck2, log)?;
    let again = output(&tool, &["check"])?;
    if !again.status.success() {
        return Err(Error::StillFails {
            status: again.status.to_string(),
            stderr: trimmed(&again.stderr),
        }
        .into());
    }
    let (patch, files) = patch()?;
    write(&handoff.patch, &patch, false)?;
    let apply = handoff.apply_command();
    write(
        &handoff.summary,
        summary_markdown(&inputs, &files, &apply).as_bytes(),
        true,
    )?;
    Err(Error::Stale {
        inputs,
        files,
        apply,
    }
    .into())
}

/// `bytes` as text, trimmed.
fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// `buildrules sync --buck2 <buck2>`, everything it prints written to `log` as it comes.
fn sync(tool: &Path, buck2: &Path, log: &mut dyn Write) -> Result<(), FarmError> {
    // One pipe for both of its streams, so its words reach the log in the order it wrote them.
    let (reader, writer) = std::io::pipe().map_err(start_error(tool))?;
    let mut child = {
        let mut command = Command::new(tool);
        command
            .arg("sync")
            .arg("--buck2")
            .arg(buck2)
            .stdin(Stdio::null())
            .stdout(writer.try_clone().map_err(start_error(tool))?)
            .stderr(writer);
        // The command, and with it this process's ends of the pipe, is dropped at the end of the
        // block, so the read below ends when sync and its children close theirs.
        command.spawn().map_err(start_error(tool))?
    };
    tee(reader, log);
    let status = child.wait().map_err(start_error(tool))?;
    if !status.success() {
        return Err(Error::Sync {
            status: status.to_string(),
        }
        .into());
    }
    Ok(())
}

/// `git <args>` in the working directory: what it printed on its standard output.
fn git(args: &[&str]) -> Result<Vec<u8>, FarmError> {
    let git = Path::new("git");
    let out = output(git, args)?;
    if !out.status.success() {
        return Err(Error::Git {
            args: args.join(" "),
            status: out.status.to_string(),
            stderr: trimmed(&out.stderr),
        }
        .into());
    }
    Ok(out.stdout)
}

/// The patch of what sync changed in the working tree, and the files it changes.
fn patch() -> Result<(Vec<u8>, Vec<String>), FarmError> {
    let mut list = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    list.extend(SYNCED);
    let untracked = git(&list)?;
    let untracked = String::from_utf8_lossy(&untracked);
    let new: Vec<&str> = untracked.split('\0').filter(|p| !p.is_empty()).collect();
    if !new.is_empty() {
        let mut add = vec!["add", "--intent-to-add", "--"];
        add.extend(&new);
        git(&add)?;
    }
    let patch = git(&[&DIFF[..], &["--binary", "--"], &SYNCED[..]].concat())?;
    let names = git(&[&DIFF[..], &["--name-only", "--"], &SYNCED[..]].concat())?;
    let files = String::from_utf8_lossy(&names)
        .lines()
        .map(str::to_owned)
        .collect();
    Ok((patch, files))
}

/// Writes `bytes` to `path`, after what it holds when `append`, making its directory.
fn write(path: &Path, bytes: &[u8], append: bool) -> Result<(), FarmError> {
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(path)
        })
        .and_then(|mut file| file.write_all(bytes));
    written.map_err(|e| {
        Error::Write {
            path: path.to_owned(),
            why: e.to_string(),
        }
        .into()
    })
}

#[cfg(test)]
mod tests;
