//! `--job-store-grant-key-file`, `--job-store-url` and `--job-store-bucket`: the grant key and the
//! store a task is told to read from, which `GET /locate` signs read grants with
//! ([`kbf_front::job_read::StoreGrants`]).
//!
//! The three come together and are off unless the key file is given. The key file is the one the
//! tier store reads for `--grant-key-file` (JSON of `access_key_id` and `secret_access_key`, or the
//! id and the secret on two lines), under the owner-only rule of the other secret files; a refusal
//! names the flag and the file, never what it holds. The store URL is `http://` or `https://` and
//! a host, with no path, query or fragment: a task appends `/<bucket>/<key>` to it.

use std::path::{Path, PathBuf};

use clap::{Arg, ArgMatches};
use kbf_front::job_read::{
    AccessKey, StoreGrantError, StoreGrants, check_store_bucket, check_store_url,
};
use zeroize::Zeroizing;

use super::{JobReadError, StoreGrantFlags};
use crate::token::read_owner_only;

const LISTEN: &str = "job_listen";
const KEY_FILE: &str = "job_store_grant_key_file";
const URL: &str = "job_store_url";
const BUCKET: &str = "job_store_bucket";

/// The most bytes the key file may hold.
const MAX_KEY_FILE_BYTES: usize = 4096;

/// The fewest bytes a grant secret may have: a grant is an HMAC over a body of known form, so whoever
/// sees one token can test guesses of the secret offline. tierstore refuses shorter ones too; the
/// check is here as well so the minimum does not move with the tierstore pin.
const MIN_SECRET_BYTES: usize = 32;

/// What a key file that is not a key is told.
const NOT_KEY: &str = "not JSON of exactly access_key_id and secret_access_key, both not \
                       empty, nor the id and the secret on two lines";

/// The three flags.
pub(super) fn args() -> [Arg; 3] {
    [
        Arg::new(KEY_FILE)
            .long("job-store-grant-key-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires_all([LISTEN, URL, BUCKET])
            .help(
                "A file with the grant key of the tier store, `{\"access_key_id\": \"...\", \
                 \"secret_access_key\": \"...\"}` (or the id on the first line and the secret on the \
                 second), owned by this user and readable by it alone (mode 0600 or 0400): the key the \
                 store's `--grant-key-file` holds. With it, `GET /locate` signs read grants for the packs \
                 and large objects its table names, each good for the Action's job and at most ten \
                 minutes, and the task reads them from the store directly with `--locate`; the store must \
                 know the same key. Without it, `GET /locate` carries no grant and the task reads every \
                 blob from this listener. The secret is never a flag. Needs `--job-store-url`, \
                 `--job-store-bucket` and `--job-listen`",
            ),
        Arg::new(URL)
            .long("job-store-url")
            .value_name("URL")
            .value_parser(clap::builder::NonEmptyStringValueParser::new())
            .requires(KEY_FILE)
            .help(
                "The base URL of the tier store that a task is told to read from with a grant, \
                 `http://host:port` or `https://host:port` with no path (the task dials it from its \
                 worker, so give an address the workers reach). Needs `--job-store-grant-key-file`",
            ),
        Arg::new(BUCKET)
            .long("job-store-bucket")
            .value_name("BUCKET")
            .value_parser(clap::builder::NonEmptyStringValueParser::new())
            .requires(KEY_FILE)
            .help(
                "The bucket of the tier store that read grants are made for, the store's `--bucket`. \
                 Needs `--job-store-grant-key-file`",
            ),
    ]
}

/// The flags `m` makes, or `None` without `--job-store-grant-key-file` (clap makes the other two
/// come with it).
pub(super) fn from_matches(m: &ArgMatches) -> Option<StoreGrantFlags> {
    let key_file = m.get_one::<PathBuf>(KEY_FILE)?.clone();
    let text = |id| {
        m.get_one::<String>(id)
            .expect("clap requires it with the key file")
            .clone()
    };
    Some(StoreGrantFlags {
        key_file,
        url: text(URL),
        bucket: text(BUCKET),
    })
}

/// The grants `flags` describe: the key read, the URL checked, the key and bucket tried.
///
/// # Errors
/// The key file cannot be used, the URL cannot be a store's, or tierstore would refuse the key
/// or the bucket.
pub(super) fn grants(flags: &StoreGrantFlags) -> Result<StoreGrants, JobReadError> {
    let key = read_key(&flags.key_file)?;
    check_store_bucket(&flags.bucket).map_err(|why| JobReadError::StoreBucket {
        reason: why.to_owned(),
    })?;
    check_store_url(&flags.url).map_err(|why| JobReadError::StoreUrl {
        why: why.to_owned(),
    })?;
    // The key and the bucket are tried by making a grant with them: tierstore's own verdict.
    StoreGrants::new(key, &flags.url, &flags.bucket).map_err(|error| match error {
        StoreGrantError::Bucket => JobReadError::StoreBucket {
            reason: error.to_string(),
        },
        other => JobReadError::StoreKey {
            path: flags.key_file.clone(),
            why: other.to_string(),
        },
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    access_key_id: String,
    secret_access_key: String,
}

/// The key in the file at `path`.
fn read_key(path: &Path) -> Result<AccessKey, JobReadError> {
    let bad = |why: String| JobReadError::StoreKey {
        path: path.to_owned(),
        why,
    };
    let bytes =
        read_owner_only(path, MAX_KEY_FILE_BYTES).map_err(|error| bad(error.to_string()))?;
    let bytes = Zeroizing::new(bytes);
    let text = std::str::from_utf8(&bytes).map_err(|_| bad(NOT_KEY.to_owned()))?;
    let (id, secret) = if text.trim_start().starts_with('{') {
        let keys = serde_json::from_str::<Keys>(text).map_err(|_| bad(NOT_KEY.to_owned()))?;
        (keys.access_key_id, keys.secret_access_key)
    } else {
        let mut lines = text.lines().map(str::trim);
        (
            lines.next().unwrap_or_default().to_owned(),
            lines.next().unwrap_or_default().to_owned(),
        )
    };
    if id.is_empty() || secret.is_empty() {
        return Err(bad(NOT_KEY.to_owned()));
    }
    if secret.len() < MIN_SECRET_BYTES {
        return Err(bad(format!(
            "the secret is shorter than {MIN_SECRET_BYTES} bytes"
        )));
    }
    Ok(AccessKey::new(&id, &secret))
}
