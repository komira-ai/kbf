//! `channels apt write --bucket <b> --distribution-id <id> --hostname <h> --next <dir> (--debs <dir>
//! --version <V> | --no-pool) [--suite stable|testing] [--last] [--output <file>] [--aws
//! <program>]`: the signed index and the packages into the apt repository.
//!
//! The order is what keeps a reader consistent when the write stops anywhere:
//!
//! 1. the packages (none with `--no-pool`: the promotion of a release from `testing` to `stable`
//!    writes the index only, the pool holds the packages already), each with `If-None-Match: *` (a `412` means it exists; it is kept only when
//!    its bytes are ours, and a different file under the name is a failure: a package is never
//!    overwritten, and the index must not list a file that is not this one);
//! 2. the by-hash copy of every `Packages` and `Packages.gz`
//!    (`main/binary-<arch>/by-hash/SHA256/<hash>`, `If-None-Match`; the `Release` says
//!    `Acquire-By-Hash: yes`), so a client with the old `Release` finds the old files and one
//!    with the new `Release` the new, whatever part of this write has happened;
//! 3. the `Packages` files, then `Release`, `Release.gpg` and `InRelease` last, each with
//!    `If-Match` on the ETag `apt-read` saw (`If-None-Match` for an object that did not exist).
//!    The `If-Match` of `InRelease` is the commit point.
//!
//! A `412` on the index means someone else wrote in between: `conflict=true` is appended to
//! `--output` and the verb ends green, so the next attempt reads again; with `--last` it fails
//! instead. Before anything is written, the index must list exactly the packages to upload (by
//! name and digest) and each `Packages` must be the file its `Release` lists. Then
//! `/apt/dists/*` is invalidated.

use std::path::Path;

use super::index::{self, sha256_hex};
use super::state::{self, ABSENT, ETAGS_FILE, POOL_DIR, Suite};
use super::{Failure, classify, control, package_files, strings, why};
use crate::channels::Error;
use crate::channels::append_output;
use crate::channels::aws::Aws;
use crate::channels::{is_bucket, is_distribution_id};

/// What `apt write` is asked.
pub struct Request<'a> {
    /// The suite of the repository.
    pub suite: Suite,
    /// The apt repository's bucket.
    pub bucket: &'a str,
    /// The CDN distribution to invalidate `/apt/dists/*` in.
    pub distribution_id: &'a str,
    /// The repository's host name, for the closing message.
    pub hostname: &'a str,
    /// The directory `apt index` wrote, signed by `apt sign`.
    pub next: &'a Path,
    /// The packages to put in the pool, or `None` to write the index only.
    pub pool: Option<Pool<'a>>,
    /// This is the last attempt: a lost race fails instead of asking for another.
    pub last: bool,
}

/// The packages of a release: the directory they are in and their version.
#[derive(Clone, Copy)]
pub struct Pool<'a> {
    /// The directory holding the package files.
    pub debs: &'a Path,
    /// The release's full commit, which names the package files.
    pub commit: &'a str,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
}

/// What one conditional put came to.
enum Put {
    Done,
    Lost,
}

/// The condition of a put: `If-Match` on an ETag that was read, or `If-None-Match` for an object
/// that does not exist.
fn condition(etag: Option<&str>) -> [String; 2] {
    match etag {
        Some(etag) if etag != ABSENT => ["--if-match".to_owned(), etag.to_owned()],
        _ => ["--if-none-match".to_owned(), "*".to_owned()],
    }
}

fn put(
    aws: &dyn Aws,
    bucket: &str,
    key: &str,
    body: &Path,
    cond: &[String; 2],
) -> Result<Put, Error> {
    let body = body.display().to_string();
    let mut args = strings(&[
        "s3api",
        "put-object",
        "--bucket",
        bucket,
        "--key",
        key,
        "--body",
        &body,
    ]);
    args.extend(cond.iter().cloned());
    let answer = aws.run(&args)?;
    if answer.ok {
        return Ok(Put::Done);
    }
    match classify(&answer.stderr) {
        Failure::Precondition => Ok(Put::Lost),
        _ => Err(Error::Failed(format!(
            "put-object {key} failed: {}",
            why(&answer)
        ))),
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(Error::spawn(format!("read {}", path.display())))
}

fn read_text(path: &Path) -> Result<String, Error> {
    read(path).and_then(|b| {
        String::from_utf8(b).map_err(|_| Error::Failed(format!("{} is not UTF-8", path.display())))
    })
}

/// The index of the next state is complete, lists the packages to upload by name and digest, and
/// has each `Packages` the `Release` says. The files it has.
fn check_next(
    r: &Request,
    packages: &[(String, std::path::PathBuf)],
) -> Result<Vec<String>, Error> {
    let root = r.next.join("dists").join(r.suite.name());
    let release = read_text(&root.join("Release"))?;
    let files = state::index_files(&state::archs_of_release(&release));
    for rel in &files {
        if !root.join(rel).is_file() {
            return Err(Error::Failed(format!(
                "{} is missing from the new index: it was not built or not signed",
                root.join(rel).display()
            )));
        }
    }
    for rel in files.iter().filter(|rel| state::is_packages_file(rel)) {
        let hash = sha256_hex(&read(&root.join(rel))?);
        match state::release_hash_of(&release, rel) {
            Some(listed) if listed == hash => {}
            Some(_) => {
                return Err(Error::Failed(format!(
                    "{rel} does not have the hash its Release lists: the index was changed after it was built"
                )));
            }
            None => {
                return Err(Error::Failed(format!(
                    "the Release lists no hash for {rel}"
                )));
            }
        }
    }
    let mut listed = Vec::new();
    for rel in files.iter().filter(|rel| rel.ends_with("/Packages")) {
        listed.extend(index::parse_packages(&read_text(&root.join(rel))?)?);
    }
    for (name, path) in packages {
        let filename = format!("{POOL_DIR}/{name}");
        let digest = sha256_hex(&read(path)?);
        let entry = listed.iter().find(|s| {
            control::parse(&s.text)
                .is_ok_and(|f| control::get(&f, "Filename") == Some(filename.as_str()))
        });
        if entry.map(|s| s.sha256.as_str()) != Some(digest.as_str()) {
            return Err(Error::Failed(format!(
                "the new index does not list {name} with the digest of the file to upload ({digest})"
            )));
        }
    }
    Ok(files)
}

/// Uploads one package to the pool, once. A `412` passes only if the stored file is ours.
fn upload_package(
    aws: &dyn Aws,
    r: &Request,
    name: &str,
    path: &Path,
    printed: &mut Vec<String>,
) -> Result<(), Error> {
    let key = state::pool_key(name)?;
    if matches!(put(aws, r.bucket, &key, path, &condition(None))?, Put::Done) {
        printed.push(format!("uploaded {key}"));
        return Ok(());
    }
    // Kept as it is, but only if it is the same file: a different one under this name is a release
    // whose bytes changed (a build that is not reproducible, or a version published twice).
    let there = r.next.join("pool-check.deb");
    let there_arg = there.display().to_string();
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        &key,
        &there_arg,
    ]))?;
    if !got.ok {
        return Err(Error::Failed(format!(
            "{key} exists already and could not be read to compare it with the file to publish: {}",
            why(&got)
        )));
    }
    let have = sha256_hex(&read(&there)?);
    let _ = std::fs::remove_file(&there);
    let want = sha256_hex(&read(path)?);
    if have != want {
        return Err(Error::Failed(format!(
            "{key} exists already with different bytes (sha256 {have} there, {want} here): a package is never overwritten, and a different file under the same name is a failure"
        )));
    }
    printed.push(format!(
        "{key} exists already, with the same bytes; it is kept as it is"
    ));
    Ok(())
}

/// The `apt write` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a new index that is incomplete or does not match the packages, a put
/// that fails for a reason other than a lost race (and a lost race on the index with `--last`), a
/// package at a name with other bytes, or an invalidation that fails.
pub fn write(aws: &dyn Aws, r: &Request, output: Option<&Path>) -> Result<Vec<String>, Error> {
    if !is_bucket(r.bucket) {
        return Err(Error::Usage(format!(
            "--bucket {:?} is not a bucket name",
            r.bucket
        )));
    }
    if !is_distribution_id(r.distribution_id) {
        return Err(Error::Usage(format!(
            "--distribution-id {:?} is not a distribution id",
            r.distribution_id
        )));
    }
    if r.hostname.is_empty()
        || !r
            .hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(Error::Usage(format!(
            "--hostname {:?} is not a host name",
            r.hostname
        )));
    }
    let packages = match r.pool {
        Some(pool) => package_files(pool.debs, pool.commit, pool.version)?,
        None => Vec::new(),
    };
    let files = check_next(r, &packages)?;
    let etags = state::parse_etags(&read_text(&r.next.join(ETAGS_FILE))?)?;
    let root = r.next.join("dists").join(r.suite.name());
    let mut printed = Vec::new();

    for (name, path) in &packages {
        upload_package(aws, r, name, path, &mut printed)?;
    }
    for rel in files.iter().filter(|rel| state::is_packages_file(rel)) {
        let path = root.join(rel);
        let key = state::by_hash_key(r.suite, rel, &sha256_hex(&read(&path)?));
        // Named by their content, so a 412 means the same bytes are there.
        match put(aws, r.bucket, &key, &path, &condition(None))? {
            Put::Done => printed.push(format!("uploaded {key}")),
            Put::Lost => printed.push(format!("{key} exists already; it is kept as it is")),
        }
    }
    for rel in &files {
        let key = state::index_key(r.suite, rel);
        let cond = condition(state::etag_of(&etags, rel));
        match put(aws, r.bucket, &key, &root.join(rel), &cond)? {
            Put::Done => printed.push(format!("wrote {key}")),
            Put::Lost if r.last => {
                return Err(Error::Failed(format!(
                    "{key} changed while the index was built, and this was the last attempt: nothing more is written"
                )));
            }
            Put::Lost => {
                if let Some(output) = output {
                    append_output(output, &["conflict=true".to_owned()])?;
                }
                printed.push(format!(
                    "::notice title=apt index::{key} changed while the index was built; the next attempt reads it again"
                ));
                return Ok(printed);
            }
        }
    }
    let invalidated = aws.run(&strings(&[
        "cloudfront",
        "create-invalidation",
        "--distribution-id",
        r.distribution_id,
        "--paths",
        "/apt/dists/*",
    ]))?;
    if !invalidated.ok {
        return Err(Error::Failed(format!(
            "the invalidation of /apt/dists/* failed: {}",
            why(&invalidated)
        )));
    }
    printed.push(format!(
        "invalidated /apt/dists/*; the index is at https://{}/apt/dists/{}/InRelease",
        r.hostname,
        r.suite.name()
    ));
    if let Some(output) = output {
        append_output(output, &["conflict=false".to_owned()])?;
    }
    Ok(printed)
}

#[cfg(test)]
mod tests;
