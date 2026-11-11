//! What the shared index consists of, and what was read of it. Pure functions over text.
//!
//! The repository is one tree of object keys under `apt/`. The index of a suite (`stable`,
//! `testing`) is a handful of objects under `apt/dists/<suite>/`: for each architecture a `Packages` and a
//! `Packages.gz`, and `Release`, `InRelease` and `Release.gpg`. Every write to one of them is
//! conditional on the version that was read (its ETag), so the ETag of each object is kept beside
//! the copy of it in a text file, `etags.txt`, one `<path><TAB><etag>` line per object, `-` for an
//! object that did not exist.

use crate::channels::Error;

/// The key prefix of everything the repository serves.
pub const KEY_ROOT: &str = "apt/";

/// The suites of the repository this product publishes into. Both list the same pool: `testing`
/// holds a release from the moment it is published, `stable` gets the same entries only after the
/// release's install check passed, so a package is never copied, only listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Suite {
    /// The suite hosts install from.
    Stable,
    /// The suite a release is published to first, and checked from.
    Testing,
}

impl Suite {
    /// The name: the `Suite` and `Codename` of the `Release`, and the directory below `dists/`.
    pub fn name(self) -> &'static str {
        match self {
            Suite::Stable => "stable",
            Suite::Testing => "testing",
        }
    }

    /// The index of the suite, relative to the repository root: `dists/<suite>/`.
    pub fn dir(self) -> String {
        format!("dists/{}/", self.name())
    }
}

/// The pool directory this product owns; its role may write below it and nowhere else in the pool.
pub const POOL_DIR: &str = "pool/kbf";

/// The package the release publishes.
pub const PACKAGE: &str = "kbf";

/// `kbf_<version>_<architecture>.deb`, the package's name in the pool.
pub fn file_name(version: &str, architecture: &str) -> String {
    format!("{PACKAGE}_{version}_{architecture}.deb")
}

/// The Debian architectures and the release platforms their packages are built for.
pub const PLATFORMS: [(&str, &str); 2] = [("amd64", "linux-x86_64"), ("arm64", "linux-arm64")];

/// `kbf-native-<commit>-<platform>.deb`, the package as the release carries it (an asset, and a
/// line of `SHA256SUMS`); its bytes are the pool file's.
pub fn asset_name(commit: &str, platform: &str) -> String {
    format!("kbf-native-{commit}-{platform}.deb")
}

/// Is `commit` a full commit: 40 lower case hex digits?
pub fn is_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The architectures that are read and written even when no `Release` lists them yet.
pub const DEFAULT_ARCHS: [&str; 2] = ["amd64", "arm64"];

/// The file of ETags next to the copy of the index.
pub const ETAGS_FILE: &str = "etags.txt";

/// The ETag of an object that did not exist when it was read.
pub const ABSENT: &str = "-";

/// The objects that sign the index, in the order they are written (after every `Packages` file).
/// `InRelease` is last: it is the file apt reads first, so its `If-Match` is the commit point of a
/// publish (everything it lists is there before it is replaced).
pub const RELEASE_FILES: [&str; 3] = ["Release", "Release.gpg", "InRelease"];

/// Is `arch` a clean architecture name (it becomes a directory and part of a key)?
pub fn valid_arch(arch: &str) -> bool {
    !arch.is_empty()
        && arch
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The architectures a `Release` lists (the `main/binary-<arch>/Packages` words of its checksums)
/// together with the defaults, each once, sorted. A word that is not a clean architecture name is
/// ignored: it is never used as a path.
pub fn archs_of_release(release: &str) -> Vec<String> {
    let mut out: Vec<String> = DEFAULT_ARCHS.iter().map(|a| (*a).to_owned()).collect();
    for word in release.split_whitespace() {
        let arch = word
            .strip_prefix("main/binary-")
            .and_then(|w| w.strip_suffix("/Packages"));
        if let Some(arch) = arch.filter(|a| valid_arch(a)) {
            out.push(arch.to_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The objects of the index of `archs`, relative to `dists/<suite>/`, in the order they are
/// written: every `Packages` and `Packages.gz` first, then `Release`, `Release.gpg`, `InRelease`.
/// A reader sees a `Release` only after the files it lists.
pub fn index_files(archs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for arch in archs {
        out.push(format!("main/binary-{arch}/Packages"));
        out.push(format!("main/binary-{arch}/Packages.gz"));
    }
    out.extend(RELEASE_FILES.iter().map(|f| (*f).to_owned()));
    out
}

/// The object key of a file of the index of `suite`.
pub fn index_key(suite: Suite, rel: &str) -> String {
    format!("{KEY_ROOT}{}{rel}", suite.dir())
}

/// Is `rel` (relative to `dists/<suite>/`) a `Packages` or `Packages.gz` file, the files a
/// `Release` lists by hash?
pub fn is_packages_file(rel: &str) -> bool {
    rel.ends_with("/Packages") || rel.ends_with("/Packages.gz")
}

/// The sha256 a `Release` lists for `rel` (a line ` <hash> <size> <rel>` of its checksums).
pub fn release_hash_of<'a>(release: &'a str, rel: &str) -> Option<&'a str> {
    release.lines().find_map(|line| {
        let mut words = line.strip_prefix(' ')?.split_whitespace();
        let (hash, _size, path) = (words.next()?, words.next()?, words.next()?);
        (words.next().is_none() && path == rel && hash.len() == 64).then_some(hash)
    })
}

/// The object key of the by-hash copy of the index file `rel` (`main/binary-<arch>/Packages[.gz]`)
/// of `suite` whose sha256 is `hash`: `apt/dists/<suite>/main/binary-<arch>/by-hash/SHA256/<hash>`,
/// the name an `Acquire-By-Hash: yes` `Release` makes apt look for.
pub fn by_hash_key(suite: Suite, rel: &str, hash: &str) -> String {
    let dir = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
    format!("{KEY_ROOT}{}{dir}/by-hash/SHA256/{hash}", suite.dir())
}

/// The object key of a package file in this product's pool directory, by the file's base name.
///
/// # Errors
/// A name with a separator or a leading dot, or one that is not a `.deb`: it would leave the
/// directory.
pub fn pool_key(file: &str) -> Result<String, Error> {
    if file.is_empty() || file.starts_with('.') || file.contains('/') || !file.ends_with(".deb") {
        return Err(Error::Failed(format!(
            "{file:?} is not the name of a package file for the pool"
        )));
    }
    Ok(format!("{KEY_ROOT}{POOL_DIR}/{file}"))
}

/// The ETag lines `<path><TAB><etag>`.
pub fn render_etags(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(path, etag)| format!("{path}\t{etag}\n"))
        .collect()
}

/// The entries of an `etags.txt`.
///
/// # Errors
/// A line that is not `<path><TAB><etag>` with both parts present.
pub fn parse_etags(text: &str) -> Result<Vec<(String, String)>, Error> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| match line.split_once('\t') {
            Some((path, etag)) if !path.is_empty() && !etag.is_empty() && !etag.contains('\t') => {
                Ok((path.to_owned(), etag.to_owned()))
            }
            _ => Err(Error::Failed(format!("{ETAGS_FILE}: bad line {line:?}"))),
        })
        .collect()
}

/// The ETag read for `path`, if the file was read.
pub fn etag_of<'a>(entries: &'a [(String, String)], path: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(p, _)| p == path)
        .map(|(_, etag)| etag.as_str())
}

#[cfg(test)]
mod tests;
