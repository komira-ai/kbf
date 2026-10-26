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
//!
//! The same holds for the flags of the handover between two frontends of one host and of
//! the admin port (`--handover-*`, `--instance`, `--envoy-*`, `--admin-listen`,
//! `--await-takeover`): a frontend on a shared Postgres state store has no process to hand
//! over to, and the flags of a unit written before the removal must still parse.

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

/// The eleven flags of the removed handover between two frontends of one host and of the
/// admin port. Values are not checked: nothing reads them, and a value that was valid
/// before must stay accepted.
#[derive(Clone, Debug, Default, Args)]
pub struct RetiredHandoverFlags {
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "PATH")]
    pub handover_socket: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "a|b")]
    pub instance: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "SECONDS")]
    pub handover_quiesce_secs: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "MILLISECONDS")]
    pub handover_follow_interval_ms: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "SECONDS")]
    pub handover_watchdog_secs: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "DNS_NAME")]
    pub handover_peer_name: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "MILLISECONDS")]
    pub envoy_check_interval_ms: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "MILLISECONDS")]
    pub envoy_check_timeout_ms: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "N")]
    pub envoy_unhealthy_threshold: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "ADDR")]
    pub admin_listen: Option<String>,
    /// Retired: no longer read, the handover was removed. Accepted and ignored for one release
    #[arg(long, hide = true)]
    pub await_takeover: bool,
}

impl RetiredHandoverFlags {
    /// One line for each retired flag that was given, naming it.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let given = [
            ("--handover-socket", self.handover_socket.is_some()),
            ("--instance", self.instance.is_some()),
            (
                "--handover-quiesce-secs",
                self.handover_quiesce_secs.is_some(),
            ),
            (
                "--handover-follow-interval-ms",
                self.handover_follow_interval_ms.is_some(),
            ),
            (
                "--handover-watchdog-secs",
                self.handover_watchdog_secs.is_some(),
            ),
            ("--handover-peer-name", self.handover_peer_name.is_some()),
            (
                "--envoy-check-interval-ms",
                self.envoy_check_interval_ms.is_some(),
            ),
            (
                "--envoy-check-timeout-ms",
                self.envoy_check_timeout_ms.is_some(),
            ),
            (
                "--envoy-unhealthy-threshold",
                self.envoy_unhealthy_threshold.is_some(),
            ),
            ("--admin-listen", self.admin_listen.is_some()),
            ("--await-takeover", self.await_takeover),
        ];
        given
            .into_iter()
            .filter(|(_, given)| *given)
            .map(|(flag, _)| format!("{flag} is no longer read: the handover was removed"))
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
    use clap::{Args as _, Command, FromArgMatches as _, Parser};

    use super::RetiredHandoverFlags;
    use crate::Args;

    /// All eleven retired handover flags, each with a value that parses.
    const HANDOVER: [&str; 11] = [
        "--handover-socket=/run/kbf/control.sock",
        "--instance=a",
        "--handover-quiesce-secs=10",
        "--handover-follow-interval-ms=200",
        "--handover-watchdog-secs=15",
        "--handover-peer-name=kbf.example.internal",
        "--envoy-check-interval-ms=1000",
        "--envoy-check-timeout-ms=1000",
        "--envoy-unhealthy-threshold=2",
        "--admin-listen=127.0.0.1:18003",
        "--await-takeover",
    ];

    fn handover(argv: &[&str]) -> RetiredHandoverFlags {
        let command = RetiredHandoverFlags::augment_args(Command::new("frontend"));
        let mut all = vec!["frontend"];
        all.extend_from_slice(argv);
        let matches = command
            .try_get_matches_from(all)
            .expect("the retired flags parse");
        RetiredHandoverFlags::from_arg_matches(&matches).expect("the retired flags are read")
    }

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

    /// Asserts that each retired handover flag given is named by exactly one warning, that
    /// none given is no warning, and that a value the removed flag would have refused is
    /// still accepted.
    #[test]
    fn each_retired_handover_flag_given_warns_once_naming_it() {
        assert!(handover(&[]).warnings().is_empty());
        let all = handover(&HANDOVER).warnings();
        assert_eq!(all.len(), HANDOVER.len(), "{all:?}");
        for given in HANDOVER {
            let flag = given.split('=').next().expect("a flag");
            let line = format!("{flag} is no longer read: the handover was removed");
            assert_eq!(all.iter().filter(|l| **l == line).count(), 1, "{all:?}");
        }
        let one = handover(&["--instance=b"]).warnings();
        assert_eq!(one.len(), 1, "{one:?}");
        let odd = handover(&["--envoy-unhealthy-threshold=many"]).warnings();
        assert_eq!(odd.len(), 1, "{odd:?}");
    }

    /// Asserts that the retired handover flags are hidden from `--help`, so they do not
    /// read as live settings, and that there are exactly the eleven.
    #[test]
    fn the_retired_handover_flags_are_hidden_from_help() {
        let command = RetiredHandoverFlags::augment_args(Command::new("frontend"));
        for arg in command.get_arguments() {
            assert!(arg.is_hide_set(), "{:?}", arg.get_long());
        }
        assert_eq!(command.get_arguments().count(), HANDOVER.len());
    }
}
