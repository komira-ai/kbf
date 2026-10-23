//! Flags the node still accepts and no longer reads.
//!
//! The node's update client was removed: a new release reaches a host as a package that
//! `kbf-updater` installs, not through orders the daemon writes for the root installer.
//! A `kbf.toml`, a unit or a launchd plist written before the removal still carries
//! `--update-dir` and `--orders-dir`, and a node that refused them would not start after
//! the upgrade. So for one release they parse, are hidden from `--help`, do nothing, and
//! each one given logs one warning naming it. The release after removes them.

use clap::Args;

/// The two flags of the removed update client. Values are not checked: nothing reads
/// them, and a value that was valid before must stay accepted.
#[derive(Clone, Debug, Default, Args)]
pub struct RetiredUpdateFlags {
    /// Retired: no longer read, the node's update client was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "DIR")]
    pub update_dir: Option<String>,
    /// Retired: no longer read, the node's update client was removed. Accepted and ignored for one release
    #[arg(long, hide = true, value_name = "DIR")]
    pub orders_dir: Option<String>,
}

impl RetiredUpdateFlags {
    /// One line for each retired flag that was given, naming it.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        [
            ("--update-dir", self.update_dir.is_some()),
            ("--orders-dir", self.orders_dir.is_some()),
        ]
        .into_iter()
        .filter(|(_, given)| *given)
        .map(|(flag, _)| format!("{flag} is no longer read: the node's update client was removed"))
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

    use super::RetiredUpdateFlags;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        retired: RetiredUpdateFlags,
    }

    fn warnings(argv: &[&str]) -> Vec<String> {
        let mut all = vec!["kbf-daemon"];
        all.extend_from_slice(argv);
        Wrapper::try_parse_from(all)
            .expect("the retired flags still parse")
            .retired
            .warnings()
    }

    #[test]
    fn each_retired_flag_given_warns_once_naming_it() {
        assert!(warnings(&[]).is_empty());
        let update = warnings(&["--update-dir=/var/lib/kbf/update"]);
        assert_eq!(update.len(), 1, "{update:?}");
        assert!(update[0].contains("--update-dir"), "{update:?}");
        assert!(update[0].contains("no longer read"), "{update:?}");
        let orders = warnings(&["--orders-dir", "/var/lib/kbf/orders"]);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert!(orders[0].contains("--orders-dir"), "{orders:?}");
        let both = warnings(&["--update-dir=/u", "--orders-dir=/o"]);
        assert_eq!(both.len(), 2, "{both:?}");
    }

    #[test]
    fn the_retired_flags_are_hidden_from_help() {
        use clap::CommandFactory as _;
        let help = Wrapper::command().render_long_help().to_string();
        assert!(!help.contains("update-dir"), "{help}");
        assert!(!help.contains("orders-dir"), "{help}");
    }
}
