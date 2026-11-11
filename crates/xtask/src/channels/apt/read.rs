//! `channels apt read --bucket <b> --out <dir> [--suite stable|testing] [--aws <program>]`: the current index, read from S3.
//!
//! `Release` first, then the `Packages` and `Packages.gz` of the architectures it lists and of the
//! defaults, then `Release.gpg` and `InRelease`. The copy is written under
//! `<dir>/dists/<suite>/` and the ETag of each object (`-` for one that does not exist) to
//! `<dir>/etags.txt`: the index a later job builds is conditional on exactly these.

use std::path::Path;

use serde_json::Value;

use super::state::{self, ABSENT, ETAGS_FILE, Suite};
use super::{Failure, classify, strings, why};
use crate::channels::Error;
use crate::channels::aws::Aws;
use crate::channels::is_bucket;

/// The ETag in the JSON `aws s3api get-object` prints.
pub(crate) fn etag_of(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout).ok()?;
    let etag = value.get("ETag")?.as_str()?;
    (!etag.is_empty() && !etag.contains(char::is_whitespace)).then(|| etag.to_owned())
}

/// Reads one object of the index to `dest`: its ETag, or `-` when there is no such object
/// (`NoSuchKey`, or `AccessDenied`: the publish role may not list the bucket, so S3 answers a
/// missing key with `AccessDenied`; a wrong grant shows at the first write instead).
fn fetch(
    aws: &dyn Aws,
    suite: Suite,
    bucket: &str,
    rel: &str,
    dest: &Path,
) -> Result<String, Error> {
    let key = state::index_key(suite, rel);
    let dir = dest.parent().unwrap_or(dest);
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let dest_arg = dest.display().to_string();
    let answer = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        bucket,
        "--key",
        &key,
        &dest_arg,
    ]))?;
    if answer.ok {
        return etag_of(&answer.stdout)
            .ok_or_else(|| Error::Failed(format!("get-object {key}: no ETag in the answer")));
    }
    match classify(&answer.stderr) {
        Failure::NotFound | Failure::Denied => {
            // Nothing was written, or a part of an error page: either way there is no file.
            let _ = std::fs::remove_file(dest);
            Ok(ABSENT.to_owned())
        }
        _ => Err(Error::Failed(format!(
            "get-object {key} failed: {}",
            why(&answer)
        ))),
    }
}

/// The `apt read` verb: the lines to print.
///
/// # Errors
/// A bucket that is not a name, an object that could not be read for a reason other than being
/// absent, or a file that could not be written.
pub fn read(aws: &dyn Aws, suite: Suite, bucket: &str, out: &Path) -> Result<Vec<String>, Error> {
    if !is_bucket(bucket) {
        return Err(Error::Usage(format!(
            "--bucket {bucket:?} is not a bucket name"
        )));
    }
    let root = out.join("dists").join(suite.name());
    let release_path = root.join("Release");
    let release_etag = fetch(aws, suite, bucket, "Release", &release_path)?;
    let release = std::fs::read_to_string(&release_path).unwrap_or_default();
    let mut entries = Vec::new();
    let mut printed = Vec::new();
    for rel in state::index_files(&state::archs_of_release(&release)) {
        let etag = if rel == "Release" {
            release_etag.clone()
        } else {
            fetch(aws, suite, bucket, &rel, &root.join(&rel))?
        };
        printed.push(format!(
            "read {rel}: {}",
            if etag == ABSENT {
                "does not exist yet"
            } else {
                &etag
            }
        ));
        entries.push((rel, etag));
    }
    let etags = out.join(ETAGS_FILE);
    std::fs::write(&etags, state::render_etags(&entries))
        .map_err(Error::spawn(format!("write {}", etags.display())))?;
    Ok(printed)
}

#[cfg(test)]
mod tests;
