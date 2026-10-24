//! Flags the server still accepts and no longer reads.
//!
//! The release poller (`--release-poll-*`) and the release store (`--releases-dir`) were
//! removed: new releases reach a host as a package (`docs/design/pull-updates.md`), not
//! through a feed the frontend polls or a store it serves. A `kbf.toml` or a unit written
//! before the removal still carries their four keys, and a server that refused them would
//! not start after the upgrade. So for one release they parse, are hidden from `--help`,
//! do nothing, and each one given logs one warning naming it. The release after removes
//! them. A release store left on disk under `--releases-dir` is never opened, changed or
//! deleted: it is the operator's data.

use std::path::PathBuf;

use clap::Args;

/// The flags of the removed release poller and release store. Values are not checked:
/// nothing reads them, and a value that was valid before must stay accepted.
#[derive(Clone, Debug, Default, Args)]
pub struct RetiredReleaseFlags {
    /// Retired: no longer read, the release poller was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "SECONDS")]
    pub release_poll_every_secs: Option<String>,
    /// Retired: no longer read, the release poller was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "OWNER/NAME")]
    pub release_poll_repo: Option<String>,
    /// Retired: no longer read, the release poller was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "REF", value_delimiter = ',')]
    pub release_poll_refs: Vec<String>,
    /// Retired: no longer read, the release store was removed; a directory left there is not touched. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "DIR")]
    pub releases_dir: Option<PathBuf>,
}

impl RetiredReleaseFlags {
    /// One line for each retired flag that was given, naming it.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let given = [
            (
                "--release-poll-every-secs",
                self.release_poll_every_secs.is_some(),
            ),
            ("--release-poll-repo", self.release_poll_repo.is_some()),
            ("--release-poll-refs", !self.release_poll_refs.is_empty()),
            ("--releases-dir", self.releases_dir.is_some()),
        ];
        given
            .into_iter()
            .filter(|(_, given)| *given)
            .map(|(flag, _)| {
                let gone = if flag == "--releases-dir" {
                    "the release store"
                } else {
                    "the release poller"
                };
                format!("{flag} is no longer read: {gone} was removed")
            })
            .collect()
    }

    /// Logs [`warnings`](Self::warnings), once at start.
    pub fn warn(&self) {
        for line in self.warnings() {
            tracing::warn!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::Args;

    fn warnings(argv: &[&str]) -> Vec<String> {
        let mut all = vec!["kbf-server"];
        all.extend_from_slice(argv);
        Args::try_parse_from(all)
            .expect("the retired flags still parse")
            .retired_release
            .warnings()
    }

    /// Catches: a flag deleted outright (an old unit would then fail to start), a flag
    /// that is accepted in silence, and one warning for three flags or none at all. Each
    /// retired flag given is named by exactly one line; none given, no line.
    #[test]
    fn each_retired_flag_given_warns_once_naming_it() {
        assert!(warnings(&[]).is_empty());
        let every = warnings(&["--release-poll-every-secs=0"]);
        assert_eq!(every.len(), 1, "{every:?}");
        assert!(every[0].contains("--release-poll-every-secs"), "{every:?}");
        assert!(every[0].contains("no longer read"), "{every:?}");
        let all = warnings(&[
            "--release-poll-every-secs=300",
            "--release-poll-repo=komira-ai/kbf",
            "--release-poll-refs=main,release",
            "--releases-dir=/var/lib/kbf/releases",
        ]);
        assert_eq!(all.len(), 4, "{all:?}");
        for name in [
            "--release-poll-every-secs",
            "--release-poll-repo",
            "--release-poll-refs",
            "--releases-dir",
        ] {
            assert_eq!(
                all.iter().filter(|l| l.contains(name)).count(),
                1,
                "{all:?}"
            );
        }
        let dir = warnings(&["--releases-dir=/var/lib/kbf/releases"]);
        assert_eq!(
            dir,
            ["--releases-dir is no longer read: the release store was removed"]
        );
    }

    /// Catches: the retired flags back in `--help` (they would read as live settings).
    #[test]
    fn the_retired_flags_are_hidden_from_help() {
        let command = <Args as clap::CommandFactory>::command();
        for arg in command.get_arguments() {
            if arg
                .get_long()
                .is_some_and(|l| l.starts_with("release-poll-") || l == "releases-dir")
            {
                assert!(arg.is_hide_set(), "{:?}", arg.get_long());
            }
        }
    }
}
