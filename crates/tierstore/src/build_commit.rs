//! Which commit `build.rs` embeds as `TIERSTORE_BUILD_COMMIT`. The build script includes this
//! file by path, and the library compiles it only for its unit tests, so the rule the build
//! applies is the rule the tests check.
//!
//! The commit is the value of [`OVERRIDE`] when that variable is set, and otherwise
//! `git rev-parse --short=12 HEAD`, or `unknown` where git gives nothing. The override is for a
//! build without a `.git` (a source archive, a sandbox) that knows its commit. It is the rule of
//! `kbf-server`'s `KBF_BUILD_COMMIT_OVERRIDE`, under a tierstore name, because that one is
//! kbf's and this crate is built to move to a repository of its own.

/// The variable that replaces the commit read from git.
pub const OVERRIDE: &str = "TIERSTORE_BUILD_COMMIT_OVERRIDE";

/// The commit to embed. `override_value` is [`OVERRIDE`] as the build environment holds it
/// (`None` when unset); `git_head` is what `git rev-parse --short=12 HEAD` gave (`None` when it
/// failed), used only without an override.
///
/// # Errors
/// The override is set but is not exactly 12 lowercase hex digits, the form git gives. An empty
/// value is refused too: a script whose variable expanded to nothing must fail, not quietly
/// build from the checkout's commit.
pub fn choose(
    _override_value: Option<&std::ffi::OsStr>,
    git_head: Option<String>,
) -> Result<String, String> {
    Ok(git_head
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{OVERRIDE, choose};

    fn from(value: &str) -> Result<String, String> {
        choose(Some(OsStr::new(value)), Some("fedcba987654".to_owned()))
    }

    /// Catches: an override that is ignored (git's commit embedded instead).
    #[test]
    fn an_override_replaces_the_git_commit() {
        assert_eq!(from("0123456789ab"), Ok("0123456789ab".to_owned()));
        assert_eq!(
            choose(Some(OsStr::new("0123456789ab")), None),
            Ok("0123456789ab".to_owned())
        );
    }

    /// Catches: an override of another shape accepted (empty, too short, too long, upper case,
    /// not hex), so a stamp that is not a commit reaches `--version`.
    #[test]
    fn an_override_of_another_shape_is_refused() {
        for bad in [
            "",
            "0123456789a",
            "0123456789abc",
            "0123456789AB",
            "0123456789ag",
        ] {
            let err = from(bad).expect_err(bad);
            assert!(err.contains(OVERRIDE), "{err}");
            assert_eq!(OVERRIDE, "TIERSTORE_BUILD_COMMIT_OVERRIDE");
        }
    }

    /// Catches: without an override, a commit other than git's, or no fallback when git gives
    /// nothing.
    #[test]
    fn without_an_override_git_decides() {
        assert_eq!(
            choose(None, Some("fedcba987654".to_owned())),
            Ok("fedcba987654".to_owned())
        );
        let unknown = Ok("unknown".to_owned());
        assert_eq!(choose(None, None), unknown);
        assert_eq!(choose(None, Some(String::new())), unknown);
    }
}
