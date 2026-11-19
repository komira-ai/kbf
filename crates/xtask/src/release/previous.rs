//! `release previous`: the release before the one being made, for the `notes` job of the
//! release workflow, which downloads its files and compares the two builds.
//!
//! The release before `--tag` is the nearest earlier stable `v*` tag that is an ancestor of
//! `--commit` and has a published release (one that is not a draft, as GitHub's release list
//! says). A tag passed over for having no published release (a release run that failed
//! after its `tag` job) is named on stderr with the reason: its files were never published,
//! so there is nothing to download or compare, and its commits are in this release's notes.
//! Prints `previous=<tag>` and `previous-commit=<sha>`, both empty when no earlier tag has a
//! published release (the first release), and appends them to `--output`.

use std::io::Write;
use std::path::PathBuf;

use super::{git, notes, tags};
use crate::Error;
use crate::build_commit::full_commit;

/// The arguments of `release previous`.
#[derive(clap::Args)]
pub struct PreviousArgs {
    /// A checkout that holds the tags and main's history.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// The repository whose releases are listed, `<owner>/<repo>` (on Actions,
    /// `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The release tag.
    #[arg(long)]
    pub tag: String,
    /// The commit being released, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// Append the two lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The `release previous` verb: the lines to print.
pub fn run(a: &PreviousArgs) -> Result<Vec<String>, Error> {
    full_commit("--commit", &a.commit)?;
    let published = tags::published_tags(&a.gh, &a.repo)?;
    let (found, passed) = notes::previous_release(&a.repo_dir, &a.tag, &a.commit, &published)?;
    let named = found.as_deref().unwrap_or("none (the first release)");
    for t in &passed {
        eprintln!(
            "xtask: {t} is tagged but has no published release (its release run did not \
             publish): passed over; the release before {} is {named}",
            a.tag
        );
    }
    let at = match &found {
        Some(prev) => git::commit_of(&a.repo_dir, prev)?,
        None => String::new(),
    };
    let lines = vec![
        format!("previous={}", found.unwrap_or_default()),
        format!("previous-commit={at}"),
    ];
    if let Some(output) = &a.output {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
            .map_err(Error::spawn(format!("append to {}", output.display())))?;
    }
    Ok(lines)
}
