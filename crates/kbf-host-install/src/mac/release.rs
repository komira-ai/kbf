//! Which release a version names. Pure: nothing is read here.
//!
//! A version of a build of `main` is `0.1.<seq>+g<12 hex digits of the commit>-<run>`
//! (`xtask`'s `pkg_version`). The 12 digits pick the one tarball
//! `kbf-<40 hex digits>-darwin-arm64.tar.gz` whose commit starts with them: among the file
//! names of the channel's directory ([`pick_named`]), or, when the channel pins its releases,
//! among the lines of its `SHA256SUMS` ([`pick`]).

/// The repository whose releases a channel takes unless it names another, and whose
/// workflow's attestation it accepts.
pub const REPO: &str = "komira-ai/kbf";

/// The workflow of [`REPO`] that builds and attests the release files.
pub const WORKFLOW: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";

/// The same pair under the name before the repository was renamed: a release attested then
/// carries it, and a rollback to such a release must verify. Accepted for kbf's own
/// repository only. Delete both once no such release is installable.
pub const FORMER_REPO: &str = "komira-ai/komira-build-farm";
pub const FORMER_WORKFLOW: &str = "komira-ai/komira-build-farm/.github/workflows/artifacts.yml";

/// The asset whose releases komira-ai/tierstore's own workflow signs now that the crate lives
/// there: the start of its file names (`tierstore-<commit>-<platform>.tar.gz`).
pub const TIERSTORE_PREFIX: &str = "tierstore";

/// The repository that builds and signs the `tierstore` asset from now on. Its attestation is
/// accepted for [`TIERSTORE_PREFIX`] files only, beside kbf's own, so that a rollback to the last
/// tierstore kbf shipped still verifies. Its source ref is `refs/heads/main`, the channel's
/// branch, as for kbf.
pub const TIERSTORE_REPO: &str = "komira-ai/tierstore";

/// The workflow of [`TIERSTORE_REPO`] that builds and attests its release files.
pub const TIERSTORE_WORKFLOW: &str = "komira-ai/tierstore/.github/workflows/artifacts.yml";

/// The start of every release file's name unless the channel names another.
pub const ARTIFACT_PREFIX: &str = "kbf";

/// Where a channel's releases come from and who must have signed them: the repository
/// (which is also the one the listing is read from), the workflow whose attestation is
/// believed, and the start of the release files' names. A channel that names none of the
/// three has [`Origin::kbf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    repository: String,
    workflow: String,
    prefix: String,
}

/// Whether `text` is `owner/name`: an owner of letters, digits and `-` (not starting with
/// `-`) and a name of letters, digits and `. _ -` (not starting with `-` or `.`, no `..`).
/// It goes into a URL and the arguments of `gh`, so nothing else is let through.
pub fn is_repository(text: &str) -> bool {
    let Some((owner, name)) = text.split_once('/') else {
        return false;
    };
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let name_ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['-', '.'])
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    owner_ok && name_ok
}

/// Whether `workflow` is a `.yml` or `.yaml` file of letters, digits and `. _ -` directly under
/// `<repository>/.github/workflows/`.
pub fn is_workflow_of(repository: &str, workflow: &str) -> bool {
    let file = workflow
        .strip_prefix(repository)
        .and_then(|rest| rest.strip_prefix("/.github/workflows/"));
    let stem = file
        .and_then(|f| f.strip_suffix(".yml").or_else(|| f.strip_suffix(".yaml")))
        .unwrap_or("");
    is_repository(repository)
        && !stem.is_empty()
        && !stem.starts_with(['-', '.'])
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Whether `text` is `owner/name/.github/workflows/<file>.yml`: a signer workflow, as the
/// allowlist's `trusted_workflows` names one.
pub fn is_signer_workflow(text: &str) -> bool {
    text.split_once("/.github/workflows/")
        .is_some_and(|(repository, _)| is_workflow_of(repository, text))
}

impl Origin {
    /// kbf's own repository, workflow and file names.
    pub fn kbf() -> Self {
        Self {
            repository: REPO.to_owned(),
            workflow: WORKFLOW.to_owned(),
            prefix: ARTIFACT_PREFIX.to_owned(),
        }
    }

    /// An origin, if `repository` is `owner/name`, `workflow` is a `.yml` or `.yaml` file
    /// directly under `<repository>/.github/workflows/`, and `prefix` is lower-case letters,
    /// digits and `-` (at most 32, not starting with `-`).
    ///
    /// # Errors
    /// Which of them is not allowed, in words.
    pub fn new(repository: &str, workflow: &str, prefix: &str) -> Result<Self, String> {
        if !is_repository(repository) {
            return Err(format!(
                "{repository:?} is not a repository (owner/name of letters, digits and . _ -)"
            ));
        }
        if !is_workflow_of(repository, workflow) {
            return Err(format!(
                "{workflow:?} is not a workflow file under {repository}/.github/workflows/"
            ));
        }
        let prefix_ok = !prefix.is_empty()
            && prefix.len() <= 32
            && !prefix.starts_with('-')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !prefix_ok {
            return Err(format!(
                "{prefix:?} is not an artifact prefix (lower-case letters, digits and -)"
            ));
        }
        Ok(Self {
            repository: repository.to_owned(),
            workflow: workflow.to_owned(),
            prefix: prefix.to_owned(),
        })
    }

    /// `owner/name`.
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// The signer workflow, `owner/name/.github/workflows/<file>`.
    pub fn workflow(&self) -> &str {
        &self.workflow
    }

    /// The start of the release files' names.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The (repository, workflow) pairs an attestation of this origin's releases may name:
    /// for kbf's own pair also its former name (the transition), else only the pair. The
    /// `tierstore` asset (by its prefix, nothing else) also accepts kbf's pairs and
    /// [`TIERSTORE_REPO`]'s, when the channel names kbf's, its former name's or tierstore's
    /// repository; a channel for any other repository gets only its own pair, and no other
    /// asset gets more.
    /// The pairs are compared whole and exactly.
    pub fn signers(&self) -> Vec<(&str, &str)> {
        let mut signers = vec![(self.repository(), self.workflow())];
        let mut add = |pair: (&'static str, &'static str)| {
            if !signers.contains(&pair) {
                signers.push(pair);
            }
        };
        let pinned = [REPO, FORMER_REPO, TIERSTORE_REPO].contains(&self.repository.as_str());
        if self.prefix == TIERSTORE_PREFIX && pinned {
            add((REPO, WORKFLOW));
            add((FORMER_REPO, FORMER_WORKFLOW));
            add((TIERSTORE_REPO, TIERSTORE_WORKFLOW));
        } else if self.is_kbf_signer() {
            add((FORMER_REPO, FORMER_WORKFLOW));
        }
        signers
    }

    /// Whether the signer is exactly kbf's own repository and workflow, which no allow-list
    /// has to name.
    pub fn is_kbf_signer(&self) -> bool {
        self.repository == REPO && self.workflow == WORKFLOW
    }
}

/// The platform of a Mac's release.
pub const PLATFORM: &str = "darwin-arm64";

/// The extension of a release tarball.
pub const TARBALL_EXTENSION: &str = "tar.gz";

/// The release's checksum file, which lists the sha256 of every other file.
pub const SUMS_NAME: &str = "SHA256SUMS";

/// How many hex digits of the commit a version carries.
pub const PREFIX_LEN: usize = 12;

/// How many hex digits a commit has.
pub const COMMIT_LEN: usize = 40;

/// The commit digits a version carries: after its last `+`, an optional `g` and exactly 12
/// lower-case hex digits, then the end or `-<revision>`. A binary says `0.1.0+g<12 hex>`; a
/// deb of the same build says `0.1.0+<12 hex>`. The two cannot be confused: `g` is not a
/// hex digit.
pub fn commit_prefix(version: &str) -> Option<&str> {
    let (_, build) = version.rsplit_once('+')?;
    let after = build.strip_prefix('g').unwrap_or(build);
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
    commit_of_name_on(PLATFORM, name)
}

/// The commit in a tarball's file name `kbf-<40 hex>-<platform>.tar.gz`.
pub fn commit_of_name_on<'a>(platform: &str, name: &'a str) -> Option<&'a str> {
    commit_of_asset_on(platform, TARBALL_EXTENSION, name)
}

/// The commit in a release file's name `kbf-<40 hex>-<platform>.<extension>`.
pub fn commit_of_asset_on<'a>(platform: &str, extension: &str, name: &'a str) -> Option<&'a str> {
    commit_of_named_asset_on(ARTIFACT_PREFIX, platform, extension, name)
}

/// [`commit_of_asset_on`] for the files of a channel whose names start with `prefix`.
pub fn commit_of_named_asset_on<'a>(
    prefix: &str,
    platform: &str,
    extension: &str,
    name: &'a str,
) -> Option<&'a str> {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(&format!("-{platform}.{extension}")))
        .filter(|commit| is_commit(commit))
}

/// The sha256 that `sums` lists for the file `name`: the lines are `<64 hex>  <name>`.
///
/// # Errors
/// [`PickError::Malformed`] for sums that are not such lines (or name a file twice),
/// [`PickError::Unknown`] when `name` is not listed.
pub fn listed_hash(sums: &str, name: &str) -> Result<String, PickError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut found = None;
    if !sums.is_empty() && !sums.ends_with('\n') {
        return Err(PickError::Malformed);
    }
    for line in sums.lines() {
        let (hash, file) = line.split_once("  ").ok_or(PickError::Malformed)?;
        let hash_ok = hash.len() == 64 && hash.bytes().all(is_hex);
        if !hash_ok || file.is_empty() || file.contains('/') || !seen.insert(file) {
            return Err(PickError::Malformed);
        }
        if file == name {
            found = Some(hash.to_owned());
        }
    }
    found.ok_or(PickError::Unknown)
}

/// The tarball of `commit`.
pub fn tarball_name(commit: &str) -> String {
    tarball_name_on(PLATFORM, commit)
}

/// The tarball of `commit` for `platform`.
pub fn tarball_name_on(platform: &str, commit: &str) -> String {
    asset_name_on(platform, TARBALL_EXTENSION, commit)
}

/// The release file `kbf-<commit>-<platform>.<extension>`.
pub fn asset_name_on(platform: &str, extension: &str, commit: &str) -> String {
    named_asset_on(ARTIFACT_PREFIX, platform, extension, commit)
}

/// [`asset_name_on`] for a channel whose names start with `prefix`.
pub fn named_asset_on(prefix: &str, platform: &str, extension: &str, commit: &str) -> String {
    format!("{prefix}-{commit}-{platform}.{extension}")
}

/// The attestation bundle of `commit`'s tarball.
pub fn bundle_name(commit: &str) -> String {
    bundle_name_on(PLATFORM, commit)
}

/// The attestation bundle of `commit`'s tarball for `platform`.
pub fn bundle_name_on(platform: &str, commit: &str) -> String {
    bundle_of(&tarball_name_on(platform, commit))
}

/// The attestation bundle of the release file `asset`.
pub fn bundle_of(asset: &str) -> String {
    format!("{asset}.sigstore.json")
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
