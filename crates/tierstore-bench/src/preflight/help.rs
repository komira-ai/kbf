//! `tierstore --help` of the pinned binary, read for the flags it has and the flags it needs.
//!
//! The text is clap's: option lines begin with the flag (`-x, --name <VALUE>` or
//! `--name <VALUE>`), and the usage line lists the required ones after `[OPTIONS]`
//! (`Usage: tierstore [OPTIONS] --hot <HOT>`); an optional flag in usage is bracketed.

use std::collections::BTreeSet;

/// The flags of a help text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelpFlags {
    /// Every flag the option lines list, as `--name`.
    pub all: BTreeSet<String>,
    /// The flags the usage line requires.
    pub required: BTreeSet<String>,
}

impl HelpFlags {
    /// Whether the binary can run with a cold tier only: `--hot` is a flag it has and does not
    /// require.
    #[must_use]
    pub fn cold_only_possible(&self) -> bool {
        self.all.contains("--hot") && !self.required.contains("--hot")
    }

    /// The flags in `wanted` the help does not list, in the order given.
    #[must_use]
    pub fn missing<'a>(&self, wanted: &'a [String]) -> Vec<&'a str> {
        wanted
            .iter()
            .filter(|flag| !self.all.contains(flag.as_str()))
            .map(String::as_str)
            .collect()
    }
}

/// `--name` from a word such as `--name`, `--name,` or `--name=<V>`; `None` for any other word.
fn flag_of(word: &str) -> Option<String> {
    let name: String = word
        .strip_prefix("--")?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!name.is_empty()).then(|| format!("--{name}"))
}

/// Reads a help text.
#[must_use]
pub fn parse_help(text: &str) -> HelpFlags {
    let mut flags = HelpFlags::default();
    for line in text.lines().map(str::trim) {
        if line.starts_with("Usage:") {
            flags
                .required
                .extend(line.split_whitespace().filter_map(flag_of));
        } else if line.starts_with('-') {
            // Only the flag words at the start: after them come the value (`<VALUE>`) or a
            // description that may name other flags.
            flags.all.extend(
                line.split_whitespace()
                    .take_while(|word| word.starts_with('-'))
                    .filter_map(flag_of),
            );
        }
    }
    flags
}
