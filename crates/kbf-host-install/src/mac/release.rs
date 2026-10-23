//! Which release a version names. Pure: nothing is read here.
//!
//! A version of a build of `main` is `0.1.<seq>+g<12 hex digits of the commit>-<run>`
//! (`xtask`'s `pkg_version`). The 12 digits pick the one tarball
//! `kbf-<40 hex digits>-darwin-arm64.tar.gz` whose commit starts with them: among the file
//! names of the channel's directory ([`pick_named`]), or, when the channel pins its releases,
//! among the lines of its `SHA256SUMS` ([`pick`]).

/// The repository whose releases a Mac takes, and whose workflow's attestation it accepts.
pub const REPO: &str = "komira-ai/kbf";

/// The platform of a Mac's release.
pub const PLATFORM: &str = "darwin-arm64";

/// How many hex digits of the commit a version carries.
pub const PREFIX_LEN: usize = 12;

/// How many hex digits a commit has.
pub const COMMIT_LEN: usize = 40;

/// The 12 hex digits after the last `+g` of `version`, when the version ends there or goes
/// on with `-<revision>`.
pub fn commit_prefix(version: &str) -> Option<&str> {
    let (_, after) = version.rsplit_once("+g")?;
    let (prefix, rest) = after.split_at_checked(PREFIX_LEN)?;
    let hex = prefix.bytes().all(is_hex);
    (hex && (rest.is_empty() || rest.starts_with('-'))).then_some(prefix)
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// Whether `text` is a full commit: 40 lower-case hex digits.
pub fn is_commit(text: &str) -> bool {
    text.len() == COMMIT_LEN && text.bytes().all(is_hex)
}

/// The commit in a tarball's file name `kbf-<40 hex>-darwin-arm64.tar.gz`.
pub fn commit_of_name(name: &str) -> Option<&str> {
    name.strip_prefix("kbf-")
        .and_then(|rest| rest.strip_suffix(&format!("-{PLATFORM}.tar.gz")))
        .filter(|commit| is_commit(commit))
}

/// The tarball of `commit`.
pub fn tarball_name(commit: &str) -> String {
    format!("kbf-{commit}-{PLATFORM}.tar.gz")
}

/// The attestation bundle of `commit`'s tarball.
pub fn bundle_name(commit: &str) -> String {
    format!("{}.sigstore.json", tarball_name(commit))
}

/// The release a version picks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub commit: String,
    /// The sha256 the sums list for the tarball.
    pub sha256: String,
}

/// Why no release was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// The sums are not `<64 hex>  <name>` lines, or name a file twice.
    Malformed,
    /// No tarball of a commit that starts with the digits.
    Unknown,
    /// Two commits start with them.
    Ambiguous,
}

/// The tarball of the commit that starts with `prefix` in `sums`.
///
/// # Errors
/// [`PickError`].
pub fn pick(sums: &str, prefix: &str) -> Result<Pick, PickError> {
    let mut names = std::collections::BTreeSet::new();
    let mut found: Vec<Pick> = Vec::new();
    if !sums.is_empty() && !sums.ends_with('\n') {
        return Err(PickError::Malformed);
    }
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").ok_or(PickError::Malformed)?;
        let hash_ok = hash.len() == 64 && hash.bytes().all(is_hex);
        if !hash_ok || name.is_empty() || name.contains('/') || !names.insert(name) {
            return Err(PickError::Malformed);
        }
        if let Some(commit) = commit_of_name(name).filter(|commit| commit.starts_with(prefix)) {
            found.push(Pick {
                commit: commit.to_owned(),
                sha256: hash.to_owned(),
            });
        }
    }
    match found.len() {
        0 => Err(PickError::Unknown),
        1 => Ok(found.remove(0)),
        _ => Err(PickError::Ambiguous),
    }
}

/// The commit that starts with `prefix` among the tarball file names `names` (a directory
/// listing; names of anything else are ignored).
///
/// # Errors
/// [`PickError::Unknown`] when none does, [`PickError::Ambiguous`] when two commits do.
pub fn pick_named<'a>(
    names: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Result<String, PickError> {
    let mut found: Vec<&str> = names
        .into_iter()
        .filter_map(commit_of_name)
        .filter(|commit| commit.starts_with(prefix))
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [] => Err(PickError::Unknown),
        [one] => Ok((*one).to_owned()),
        _ => Err(PickError::Ambiguous),
    }
}
