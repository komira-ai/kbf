//! The Homebrew cask for kbf-installer, rendered with the release's version, URL and
//! SHA-256 filled in.
//!
//! The cask is the Homebrew path for a Mac that joins a farm: it installs the `.pkg`
//! ([`super::build`]), which places the root tool and a LaunchDaemon that is not loaded,
//! and its `uninstall` stanzas unload the job and forget the package's files. There is
//! no formula (contract section 7): a formula installs into the user's prefix, and the
//! root installer is refused from a user-owned prefix.
//!
//! The cask is Ruby, which a repository file would have to be listed in the interpreter
//! budget for; it is text here, and a file only in a tap, where Homebrew reads it.

use super::{GATE, IDENTIFIER, LOG_DIR, PREFIX, PROGRAM, pkg_name, valid_version};
use crate::fetch::Sha256;

const HOMEPAGE: &str = "https://github.com/komira-ai/kbf";

const CASK: &str = r#"cask "kbf-installer" do
  version "@VERSION@"
  sha256 "@SHA256@"

  url "@URL@"
  name "kbf-installer"
  desc "Sets up and updates a kbf build-farm node"
  homepage "@HOMEPAGE@"

  depends_on arch: :arm64

  pkg "@PKG@"

  uninstall launchctl: "@LABEL@",
            pkgutil:   "@LABEL@"

  zap trash: [
    "@PREFIX@",
    "@GATE@",
    "@STATE_DIR@",
    "@LOG_DIR@",
  ]

  caveats <<~EOS
    kbf-installer is installed and its service is not loaded. Nothing starts until
    the node is configured: run
    `sudo @PROGRAM@ setup`.
  EOS
end
"#;

/// What a URL may hold: the characters of a release URL, none of which ends a Ruby string
/// or starts an interpolation.
fn valid_url(url: &str) -> bool {
    // `&` and `|`, not `&&` and `||`: each comparison is judged on every URL, so the
    // tests need no URL for each short-circuit.
    (url.starts_with("https://") | url.starts_with("file:///"))
        & url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() | "._~:/%+=@,-".contains(c))
}

/// `template` with its `@NAME@` marks replaced.
fn fill(template: &str, marks: &[(&str, &str)]) -> String {
    marks
        .iter()
        .fold(template.to_owned(), |text, (name, value)| {
            text.replace(&format!("@{name}@"), value)
        })
}

/// What is wrong with the arguments of a render: one line each.
fn problems(version: &str, url: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !valid_version(version) {
        problems.push(format!("version {version:?} is not a version"));
    }
    if !valid_url(url) {
        problems.push(format!(
            "url {url:?} is not an https:// or file:/// URL of plain characters"
        ));
    }
    problems
}

/// The cask for the package ([`pkg_name`]) whose bytes have `sha256`, downloaded from
/// `url`.
///
/// # Errors
/// `version` or `url` is not plain text for a Ruby string.
pub fn cask(version: &str, sha256: &Sha256, url: &str) -> Result<String, Vec<String>> {
    let problems = problems(version, url);
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(fill(
        CASK,
        &[
            ("VERSION", version),
            ("SHA256", &sha256.to_string()),
            ("URL", url),
            ("HOMEPAGE", HOMEPAGE),
            // The cask names the package by its version, as Homebrew's own casks do.
            ("PKG", &pkg_name("#{version}")),
            ("LABEL", IDENTIFIER),
            ("PREFIX", &format!("/{PREFIX}")),
            ("PROGRAM", PROGRAM),
            ("GATE", GATE),
            ("STATE_DIR", "/var/db/kbf-installer"),
            ("LOG_DIR", &format!("/{LOG_DIR}")),
        ],
    ))
}
