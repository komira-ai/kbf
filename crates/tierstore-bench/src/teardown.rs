//! `verify-teardown`: PASS or FAIL per check, each from the before-snapshot preflight took and
//! the state now. Read-only; it removes nothing (engineering removes the test buckets, the
//! accounts and the test instance, and this verb proves it was done).
//!
//! It reads the ops file (`ops.json` of the result directory) and the snapshot named by
//! `--before`, and checks:
//! - `test-bucket-removed:<role>`: the verifier's key pair (the ops file's `verifier` entry) reads
//!   the store's bucket list (`ListBuckets`) and the test bucket is not in it. A list that cannot
//!   be read fails the check, and so does a refusal on the bucket itself; a `NoSuchBucket` on the
//!   bucket is noted and never decides alone. A store lists a key without `s3:ListAllMyBuckets`
//!   only the buckets that key may use, so a mis-scoped verifier sees an empty list and "the test
//!   bucket is not named" would prove nothing. The list is therefore trusted only if it names
//!   the role's live bucket (the ops file's `live_buckets`, which exists and is never removed):
//!   a list that does not, or a role with no live bucket configured, fails the check;
//! - `no-multipart-uploads:<role>`: no upload is open (a bucket the list does not name holds
//!   none; an uploads listing that works counts whether or not the objects listing does);
//! - `scoped-account-gone:<role>`: a request signed with the scoped account's key is refused as
//!   from an unknown access key;
//! - `request-log-test-buckets-only`: every record of `requests.jsonl` names the test bucket of
//!   its role, and every line reads. This is the harness-side proof that no request named a live
//!   bucket; it is not the store's own log, which is not enabled on the live stores;
//! - `test-unit-gone`, `test-port-free`, `test-dirs-gone`: nothing of the test instance is left;
//! - `live-binary-unchanged`, `live-unit-unchanged`: the live tierstore's binary digest and its
//!   unit's properties ([`crate::preflight::live::STABLE_PROPERTIES`]) equal the snapshot's.
//!
//! The checks are printed and written to `teardown.json`; the verb ends in an error if one
//! failed. Nothing here looks at the live buckets' contents or configuration.

use std::path::Path;

use crate::config::{RunConfig, StoreSpec};
use crate::error::BenchError;
use crate::preflight::host::{Host, SystemHost};
use crate::preflight::ops::{OpsFile, read_key_pair};
use crate::preflight::{Snapshot, all_passed, check, live, print_checks, why, write_json};
use crate::provision::SHARED_ROLES;
use crate::provision::probe::{self, Access, BucketState, Reading};
use crate::requestlog::RequestRecord;
use crate::result::Check;
use crate::role::Role;

/// The file of the result directory the checks are written to.
pub const CHECKS_FILE: &str = "teardown.json";

/// The request log of the result directory, which `run` writes.
pub const REQUEST_LOG: &str = "requests.jsonl";

/// The text of one of the tool's own files in the result directory, read from disk: unlike the
/// node's files these are not behind the [`Host`].
fn read_own(path: &Path) -> Result<String, BenchError> {
    std::fs::read_to_string(path).map_err(|source| BenchError::Io {
        path: path.to_owned(),
        source,
    })
}

/// `Ok` if the before-snapshot's `key` equals what is read now.
fn unchanged(before: &Snapshot, key: &str, now: Result<String, String>) -> Result<String, String> {
    let was = before
        .facts
        .get(key)
        .ok_or_else(|| format!("the before-snapshot has no {key}"))?;
    let now = now?;
    if *was == now {
        Ok(format!("sha256 {now}"))
    } else {
        Err(format!("was {was}, is {now}"))
    }
}

/// Reads the request log: every record must name the test bucket of its role.
fn log_check(config: &RunConfig) -> Result<String, String> {
    let path = config.result_dir.join(REQUEST_LOG);
    let text = read_own(&path).map_err(|e| why(&e))?;
    let (mut total, mut outside, mut unreadable) = (0_usize, 0_usize, 0_usize);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        total += 1;
        match serde_json::from_str::<RequestRecord>(line) {
            Ok(record) => {
                let own = config
                    .stores
                    .get(&record.role)
                    .is_some_and(|spec| spec.bucket == record.bucket);
                outside += usize::from(!own);
            }
            Err(_) => unreadable += 1,
        }
    }
    if outside == 0 && unreadable == 0 {
        Ok(format!(
            "{total} requests, every one to its role's test bucket"
        ))
    } else {
        Err(format!(
            "{total} requests: {outside} to a bucket that is not the test bucket of its role, {unreadable} unreadable lines"
        ))
    }
}

/// `spec` with the verifier's key pair.
fn verifier_spec(
    host: &dyn Host,
    ops: &OpsFile,
    role: Role,
    spec: &StoreSpec,
) -> Result<StoreSpec, String> {
    let file = ops
        .verifier
        .get(&role)
        .ok_or_else(|| "the ops file names no verifier key file".to_owned())?;
    let key = read_key_pair(host, file).map_err(|e| why(&e))?;
    Ok(StoreSpec {
        access_key_id: key.access_key_id,
        secret_access_key: key.secret_access_key,
        ..spec.clone()
    })
}

/// `Ok` if the verifier's bucket list names the role's live bucket, which exists: only then does
/// a list that does not name the test bucket mean the bucket is gone, and not that the key was
/// shown nothing.
///
/// `names` is the list that decided "not named" (it is not asked for again: a second list could
/// differ from the first).
fn list_shows_live(
    ops: &OpsFile,
    role: Role,
    names: &Result<Vec<String>, String>,
) -> Result<(), String> {
    let live = ops.live_buckets.get(&role).ok_or_else(|| {
        format!(
            "the ops file names no live bucket for {role}, so the bucket list cannot prove removal"
        )
    })?;
    if names.as_ref().is_ok_and(|names| names.contains(live)) {
        Ok(())
    } else {
        Err(format!(
            "the verifier's bucket list does not show the live bucket {live}: the key lacks s3:ListAllMyBuckets, so the list cannot prove removal"
        ))
    }
}

/// What a reading says of the entries it counted: `3 objects`, or why it did not count.
fn counted(reading: &Reading, noun: &str) -> String {
    match reading {
        Reading::Count(n) => format!("{n} {noun}"),
        Reading::NoSuchBucket => format!("{noun} not read (status 404 NoSuchBucket)"),
        Reading::Failed(reason) => format!("{noun} not read ({reason})"),
    }
}

/// `test-bucket-removed`: `Ok` only if the account listing was read and does not name the bucket.
/// A refusal on the bucket itself is never read as removal, and a `NoSuchBucket` on it is only
/// noted beside a listing that does not name it.
fn removed(state: &BucketState) -> Result<String, String> {
    match &state.listed {
        Err(reason) => Err(format!(
            "cannot tell: the bucket list cannot be read: {reason}"
        )),
        Ok(true) => Err(format!(
            "the bucket is listed: {}, {}",
            counted(&state.objects, "objects"),
            counted(&state.uploads, "uploads")
        )),
        Ok(false) if matches!(state.objects, Reading::Count(n) if n > 0) => Err(format!(
            "the bucket is not listed but answers with objects: {}",
            counted(&state.objects, "objects")
        )),
        Ok(false) if state.objects == Reading::NoSuchBucket => Ok(
            "the store's bucket list does not name it (the bucket itself answers NoSuchBucket)"
                .to_owned(),
        ),
        Ok(false) => Ok("the store's bucket list does not name it".to_owned()),
    }
}

/// `no-multipart-uploads`: the uploads listing decides when it was read, whether or not the
/// bucket's other listings were; a bucket the account listing does not name holds none.
fn uploads_closed(state: &BucketState) -> Result<String, String> {
    match &state.uploads {
        Reading::Count(0) => Ok("no upload is open".to_owned()),
        Reading::Count(open) => Err(format!("{open} uploads are open")),
        _ if state.listed == Ok(false) => Ok("the bucket is gone, so are its uploads".to_owned()),
        Reading::NoSuchBucket => Err("cannot tell: status 404 NoSuchBucket".to_owned()),
        Reading::Failed(reason) => Err(format!("cannot tell: {reason}")),
    }
}

/// The three checks of one shared store's test bucket and account.
async fn store_checks(host: &dyn Host, ops: &OpsFile, role: Role, spec: &StoreSpec) -> Vec<Check> {
    let reader = verifier_spec(host, ops, role, spec);
    let (state, names) = match &reader {
        Ok(reader) => match probe::bucket_state_and_names(reader).await {
            Ok((state, names)) => (Ok(state), names),
            Err(e) => (Err(why(&e)), Err(why(&e))),
        },
        Err(reason) => (Err(reason.clone()), Err(reason.clone())),
    };
    let mut removed = state.clone().and_then(|state| removed(&state));
    if removed.is_ok()
        && let Err(reason) = list_shows_live(ops, role, &names)
    {
        // "Not named" counts only from a list that names the live bucket.
        removed = Err(reason);
    }
    let uploads = state.and_then(|state| uploads_closed(&state));
    let account =
        probe::can_list(spec)
            .await
            .map_err(|e| why(&e))
            .and_then(|access| match access {
                Access::UnknownKey => Ok("the store does not know the access key".to_owned()),
                other => Err(format!("the account still answers: {other:?}")),
            });
    vec![
        check(&format!("test-bucket-removed:{role}"), removed),
        check(&format!("no-multipart-uploads:{role}"), uploads),
        check(&format!("scoped-account-gone:{role}"), account),
    ]
}

/// The checks of what is left on this node: the test instance, and the live tierstore.
fn node_checks(host: &dyn Host, ops: &OpsFile, before: &Snapshot) -> Vec<Check> {
    let instance = &ops.test_instance;
    let unit = live::unit_absent(host, &instance.unit)
        .map_err(|e| why(&e))
        .and_then(|absent| {
            if absent {
                Ok("the unit is not loaded".to_owned())
            } else {
                Err("the unit is still loaded".to_owned())
            }
        });
    let port = if host.port_open(instance.port) {
        Err("the port still accepts connections".to_owned())
    } else {
        Ok("nothing accepts connections on the port".to_owned())
    };
    let left = instance.dirs.iter().filter(|dir| host.exists(dir)).count();
    let dirs = if left == 0 {
        Ok(format!(
            "none of {} directories exists",
            instance.dirs.len()
        ))
    } else {
        Err(format!(
            "{left} of {} directories exist",
            instance.dirs.len()
        ))
    };
    let binary = crate::preflight::digest_of(host, &ops.live.binary);
    let unit_now = live::read_unit(host, &ops.live.unit).map_err(|e| why(&e));
    vec![
        check("test-unit-gone", unit),
        check("test-port-free", port),
        check("test-dirs-gone", dirs),
        check(
            "live-binary-unchanged",
            unchanged(before, "live_binary_sha256", binary),
        ),
        check(
            "live-unit-unchanged",
            unchanged(
                before,
                "live_unit_show_sha256",
                unit_now.map(|view| view.show_sha256),
            ),
        ),
    ]
}

/// Runs every check and returns them. Writes nothing.
///
/// # Errors
/// [`BenchError`] if the snapshot cannot be read.
pub async fn gather(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    before: &Path,
) -> Result<Vec<Check>, BenchError> {
    let text = read_own(before)?;
    let snapshot: Snapshot = serde_json::from_str(&text)
        .map_err(|e| BenchError::Ops(format!("before-snapshot: {e}")))?;
    let mut checks = Vec::new();
    for role in SHARED_ROLES {
        if let Some(spec) = config.stores.get(&role) {
            checks.extend(store_checks(host, ops, role, spec).await);
        }
    }
    checks.push(check("request-log-test-buckets-only", log_check(config)));
    checks.extend(node_checks(host, ops, &snapshot));
    Ok(checks)
}

/// Runs every check against the snapshot file `before` that preflight wrote, prints them,
/// writes `teardown.json` and ends in an error if one failed.
///
/// # Errors
/// [`BenchError`] if the ops file or the snapshot cannot be read, and [`BenchError::Ops`] if a
/// check failed (the file is written first).
pub async fn run(config: &RunConfig, before: &Path) -> Result<Vec<Check>, BenchError> {
    let host = SystemHost;
    let ops = OpsFile::load(&host, &config.result_dir)?;
    run_with(&host, config, &ops, before).await
}

/// [`run`] on a given host and ops file.
///
/// # Errors
/// As [`run`], but for the ops file.
pub async fn run_with(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    before: &Path,
) -> Result<Vec<Check>, BenchError> {
    let checks = gather(host, config, ops, before).await?;
    write_json(&config.result_dir.join(CHECKS_FILE), &checks)?;
    print_checks(&checks);
    all_passed("verify-teardown", &checks)?;
    Ok(checks)
}
