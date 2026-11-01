//! `verify-teardown`: a run that was cleaned up passes every check, and each thing left behind
//! fails the check that names it.

mod ops_support;

use std::fs;
use std::path::{Path, PathBuf};

use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::gather as preflight_gather;
use tierstore_bench::requestlog::RequestRecord;
use tierstore_bench::result::Check;
use tierstore_bench::role::Role;
use tierstore_bench::teardown::{CHECKS_FILE, gather, run, run_with};

use ops_support::*;

/// What the store says of requests for the test bucket itself, to the verifier's key.
#[derive(Clone, Copy)]
enum Bucket {
    /// `NoSuchBucket` to everything.
    Gone,
    Present {
        objects: usize,
        uploads: usize,
    },
    /// `AccessDenied` to everything.
    Refused,
    /// The objects listing is refused; the uploads listing works.
    ObjectsRefused {
        uploads: usize,
    },
}

/// What the account listing (`ListBuckets`) says to the verifier's key.
#[derive(Clone, Copy)]
enum Listing {
    /// The test buckets are not in it.
    Absent,
    /// The test buckets are in it.
    Has,
    /// `AccessDenied`.
    Denied,
    /// The list is empty: a key without `s3:ListAllMyBuckets` sees only the buckets it may use,
    /// and the live buckets are not among them.
    Blind,
}

/// How the store is: the test bucket as the verifier sees it, what the account listing says, and
/// whether the scoped accounts are known to it.
#[derive(Clone, Copy)]
struct World {
    bucket: Bucket,
    listing: Listing,
    account_known: bool,
}

const CLEAN: World = World {
    bucket: Bucket::Gone,
    listing: Listing::Absent,
    account_known: false,
};

/// The answer to `ListBuckets`: a live bucket always, the test buckets if `has`.
fn bucket_list(has: bool) -> (u16, String) {
    let mut names = vec!["live-cold-bucket-mark", "live-hot-bucket-mark"];
    if has {
        names.extend(["tb-cold-test-bucket-mark", "tb-hot-test-bucket-mark"]);
    }
    named(&names)
}

/// The answer to `ListBuckets` that names exactly `names`.
fn named(names: &[&str]) -> (u16, String) {
    let items: String = names
        .iter()
        .map(|n| format!("<Bucket><Name>{n}</Name><CreationDate>2026-10-29T10:00:00.000Z</CreationDate></Bucket>"))
        .collect();
    (
        200,
        format!(
            "<ListAllMyBucketsResult><Owner><ID>o</ID></Owner><Buckets>{items}</Buckets></ListAllMyBucketsResult>"
        ),
    )
}

/// A store that answers by who asks: the scoped accounts (`KEYID-MARK-*`) are unknown or served,
/// the verifier (`READER-KEYID-MARK`) sees the account listing and the bucket as `world` says.
fn store(world: World) -> impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static {
    move |seen| {
        if seen.key_id().starts_with("tb-KEYID-MARK") {
            return if world.account_known {
                empty_listing()
            } else {
                s3_error(403, "InvalidAccessKeyId")
            };
        }
        if seen.path == "/" {
            return match world.listing {
                Listing::Absent => bucket_list(false),
                Listing::Has => bucket_list(true),
                Listing::Blind => named(&[]),
                Listing::Denied => s3_error(403, "AccessDenied"),
            };
        }
        match world.bucket {
            Bucket::Gone => s3_error(404, "NoSuchBucket"),
            Bucket::Refused => s3_error(403, "AccessDenied"),
            Bucket::Present { objects, uploads } => {
                if is_uploads(seen) {
                    uploads_of(uploads)
                } else {
                    listing_of(objects)
                }
            }
            Bucket::ObjectsRefused { uploads } => {
                if is_uploads(seen) {
                    uploads_of(uploads)
                } else {
                    s3_error(403, "AccessDenied")
                }
            }
        }
    }
}

fn record(role: Role, bucket: &str) -> String {
    serde_json::to_string(&RequestRecord {
        unix_ms: 1,
        role,
        method: "GET".to_owned(),
        bucket: bucket.to_owned(),
        key: "k".to_owned(),
        status: Some(200),
        latency_us: 1,
        bytes: 0,
        error_kind: None,
    })
    .unwrap()
}

fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
}

fn failing(checks: &[Check]) -> Vec<&str> {
    checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect()
}

/// The reader key files, the before-snapshot a preflight of the healthy host wrote, and a request
/// log of requests to the test buckets only. Returns the path of the snapshot.
fn prepare(host: &FakeHost, dir: &Path) -> PathBuf {
    let reader =
        r#"{"access_key_id": "READER-KEYID-MARK", "secret_access_key": "READER-SECRET-MARK"}"#;
    host.file("/private-mark/reader-cold.json", reader)
        .file("/private-mark/reader-hot.json", reader);
    let config = run_config(dir, "http://host-mark:9000", Some("http://host-mark:9001"));
    let report = preflight_gather(host, &config, &ops_from(&ops_json()));
    let before = dir.join("before.json");
    fs::write(&before, serde_json::to_string(&report.snapshot).unwrap()).unwrap();
    write_log(
        dir,
        &[
            record(Role::Cold, "tb-cold-test-bucket-mark"),
            record(Role::Hot, "tb-hot-test-bucket-mark"),
        ],
    );
    before
}

fn write_log(dir: &Path, lines: &[String]) {
    fs::write(dir.join("requests.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// A machine and a store after a teardown, with the before-snapshot of the same machine.
struct After {
    dir: kbf_testkit::dir::TestDir,
    host: FakeHost,
    ops: tierstore_bench::preflight::ops::OpsFile,
    config: tierstore_bench::config::RunConfig,
    before: PathBuf,
    _stub: Stub,
}

async fn after(name: &str, world: World) -> After {
    after_on(name, Stub::start(store(world)).await).await
}

/// [`after`] on a store that is already running.
async fn after_on(name: &str, stub: Stub) -> After {
    let (dir, host, _c, ops) = base(name);
    let before = prepare(&host, dir.path());
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    After {
        dir,
        host,
        ops,
        config,
        before,
        _stub: stub,
    }
}

impl After {
    fn stub(&self) -> &Stub {
        &self._stub
    }

    async fn checks(&self) -> Vec<Check> {
        gather(&self.host, &self.config, &self.ops, &self.before)
            .await
            .unwrap()
    }
}

/// Catches a check missing from the run, and a clean teardown that fails: the store has no
/// bucket and does not know the keys, the machine has no test instance, the live unit is as it
/// was, and the log names test buckets only.
#[tokio::test]
async fn a_clean_teardown_passes_every_check() {
    let a = after("tb-down-clean", CLEAN).await;
    let checks = a.checks().await;
    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "test-bucket-removed:hot",
            "no-multipart-uploads:hot",
            "scoped-account-gone:hot",
            "test-bucket-removed:cold",
            "no-multipart-uploads:cold",
            "scoped-account-gone:cold",
            "request-log-test-buckets-only",
            "test-unit-gone",
            "test-port-free",
            "test-dirs-gone",
            "live-binary-unchanged",
            "live-unit-unchanged",
        ]
    );
    assert!(failing(&checks).is_empty(), "{checks:?}");
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "the store's bucket list does not name it (the bucket itself answers NoSuchBucket)"
    );
    assert_eq!(
        find(&checks, "request-log-test-buckets-only").detail,
        "2 requests, every one to its role's test bucket"
    );
}

/// Catches a bucket left behind passing as removed, and uploads left open passing as none: a
/// bucket with objects fails "removed" and passes "uploads"; a bucket with an open upload fails
/// both, and the detail counts.
#[tokio::test]
async fn a_bucket_with_objects_or_uploads_left_fails_the_checks_that_name_them() {
    let a = after(
        "tb-down-objects",
        World {
            bucket: Bucket::Present {
                objects: 3,
                uploads: 0,
            },
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "the bucket is listed: 3 objects, 0 uploads"
    );
    assert_eq!(
        find(&checks, "no-multipart-uploads:cold").detail,
        "no upload is open"
    );
    assert!(find(&checks, "no-multipart-uploads:cold").passed);

    let a = after(
        "tb-down-uploads",
        World {
            bucket: Bucket::Present {
                objects: 0,
                uploads: 2,
            },
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "no-multipart-uploads:cold").detail,
        "2 uploads are open"
    );
    assert_eq!(
        failing(&checks),
        [
            "test-bucket-removed:hot",
            "no-multipart-uploads:hot",
            "test-bucket-removed:cold",
            "no-multipart-uploads:cold"
        ]
    );
}

/// Catches a bucket that cannot be read passing: when the verifier is refused, both bucket
/// checks fail and say why, in the store's words and not in a path or a name.
#[tokio::test]
async fn a_bucket_that_cannot_be_read_fails_both_bucket_checks() {
    let a = after(
        "tb-down-refused",
        World {
            bucket: Bucket::Refused,
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "the bucket is listed: objects not read (status 403 AccessDenied), uploads not read (status 403 AccessDenied)"
    );
    assert_eq!(
        find(&checks, "no-multipart-uploads:cold").detail,
        "cannot tell: status 403 AccessDenied"
    );
    assert!(!find(&checks, "test-bucket-removed:cold").passed);
    assert!(!find(&checks, "no-multipart-uploads:cold").passed);

    // An uploads listing that fails after the objects listing worked.
    let stub = Stub::start(|seen| {
        if seen.path == "/" {
            bucket_list(true)
        } else if is_uploads(seen) {
            s3_error(403, "AccessDenied")
        } else {
            empty_listing()
        }
    })
    .await;
    let (dir, host, _c, ops) = base("tb-down-uploads-refused");
    let before = prepare(&host, dir.path());
    let config = run_config(dir.path(), &stub.endpoint, None);
    let checks = gather(&host, &config, &ops, &before).await.unwrap();
    assert_eq!(
        find(&checks, "no-multipart-uploads:cold").detail,
        "cannot tell: status 403 AccessDenied"
    );
}

/// Catches the verifier rule: the check passes only when the account listing, read with the
/// verifier's key, does not name the bucket. Listed fails; absent passes (and the bucket's own
/// `NoSuchBucket` is only noted); a listing that cannot be read fails, whatever the bucket itself
/// says, and `AccessDenied` on the bucket never reads as removed; a bucket that answers
/// `NoSuchBucket` while the listing names it fails.
#[tokio::test]
async fn a_bucket_is_removed_only_when_the_account_listing_does_not_name_it() {
    let name = "test-bucket-removed:cold";

    let a = after("tb-down-rule-absent", CLEAN).await;
    let checks = a.checks().await;
    let check = find(&checks, name);
    assert!(check.passed, "{check:?}");
    assert_eq!(
        check.detail,
        "the store's bucket list does not name it (the bucket itself answers NoSuchBucket)"
    );
    let asked = a.stub().requests();
    assert!(
        asked
            .iter()
            .any(|r| r.path == "/" && r.method == "GET" && r.key_id() == "READER-KEYID-MARK"),
        "the verifier's key must ask for the bucket list"
    );

    // Absent from the listing, and the bucket itself refuses: still removed, by the listing.
    let a = after(
        "tb-down-rule-absent-refused",
        World {
            bucket: Bucket::Refused,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(find(&checks, name).passed);
    assert_eq!(
        find(&checks, name).detail,
        "the store's bucket list does not name it"
    );

    let a = after(
        "tb-down-rule-listed",
        World {
            bucket: Bucket::Present {
                objects: 0,
                uploads: 0,
            },
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(!find(&checks, name).passed);
    assert_eq!(
        find(&checks, name).detail,
        "the bucket is listed: 0 objects, 0 uploads"
    );

    // The listing cannot be read: fail, whether the bucket says NoSuchBucket or AccessDenied.
    for (label, bucket) in [("nosuch", Bucket::Gone), ("denied", Bucket::Refused)] {
        let a = after(
            &format!("tb-down-rule-unreadable-{label}"),
            World {
                bucket,
                listing: Listing::Denied,
                ..CLEAN
            },
        )
        .await;
        let checks = a.checks().await;
        assert!(!find(&checks, name).passed, "{label}");
        assert_eq!(
            find(&checks, name).detail,
            "cannot tell: the bucket list cannot be read: status 403 AccessDenied",
            "{label}"
        );
    }

    // NoSuchBucket on the bucket does not outvote a listing that names it.
    let a = after(
        "tb-down-rule-listed-nosuch",
        World {
            bucket: Bucket::Gone,
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(!find(&checks, name).passed);
    assert_eq!(
        find(&checks, name).detail,
        "the bucket is listed: objects not read (status 404 NoSuchBucket), uploads not read (status 404 NoSuchBucket)"
    );
}

/// Catches the verifier's blind spot: a key without `s3:ListAllMyBuckets` is shown only the
/// buckets it may use, so a mis-scoped verifier sees an empty list and "not named" proves nothing.
/// The list is trusted only if it names the role's live bucket (the ops file's `live_buckets`):
/// a list naming neither bucket fails and says why; one naming the live bucket and not the test
/// bucket passes; one naming both fails as listed; no live bucket configured fails, and says so.
#[tokio::test]
async fn a_list_that_does_not_show_the_live_bucket_cannot_prove_removal() {
    let name = "test-bucket-removed:cold";

    let a = after(
        "tb-down-blind",
        World {
            listing: Listing::Blind,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(!find(&checks, name).passed, "{checks:?}");
    assert_eq!(
        find(&checks, name).detail,
        "the verifier's bucket list does not show the live bucket live-cold-bucket-mark: the key lacks s3:ListAllMyBuckets, so the list cannot prove removal"
    );
    assert_eq!(
        find(&checks, "test-bucket-removed:hot").detail,
        "the verifier's bucket list does not show the live bucket live-hot-bucket-mark: the key lacks s3:ListAllMyBuckets, so the list cannot prove removal"
    );

    // The live bucket named, the test bucket not: removal is proved.
    let a = after("tb-down-live-named", CLEAN).await;
    let checks = a.checks().await;
    assert!(find(&checks, name).passed, "{checks:?}");

    // Both named: the test bucket is still there.
    let a = after(
        "tb-down-both-named",
        World {
            bucket: Bucket::Present {
                objects: 0,
                uploads: 0,
            },
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(!find(&checks, name).passed);
    assert_eq!(
        find(&checks, name).detail,
        "the bucket is listed: 0 objects, 0 uploads"
    );

    // No live bucket for the role: the guard cannot be applied, which is a failure.
    let a = after("tb-down-no-live", CLEAN).await;
    let mut ops = a.ops.clone();
    ops.live_buckets.remove(&Role::Cold);
    let checks = gather(&a.host, &a.config, &ops, &a.before).await.unwrap();
    assert!(!find(&checks, name).passed);
    assert_eq!(
        find(&checks, name).detail,
        "the ops file names no live bucket for cold, so the bucket list cannot prove removal"
    );
    assert!(find(&checks, "test-bucket-removed:hot").passed);
}

/// Catches an existing bucket turned into "cannot tell" when the verifier may list uploads: with
/// the account listing naming the bucket (or unreadable) and the uploads listing granted, the
/// uploads check reads the uploads, passing on none and failing on some, even when the objects
/// listing is refused.
#[tokio::test]
async fn the_uploads_check_reads_uploads_when_that_listing_is_granted() {
    let name = "no-multipart-uploads:cold";
    for (label, listing) in [("listed", Listing::Has), ("unlisted", Listing::Denied)] {
        let a = after(
            &format!("tb-down-uploads-granted-{label}"),
            World {
                bucket: Bucket::ObjectsRefused { uploads: 0 },
                listing,
                ..CLEAN
            },
        )
        .await;
        let checks = a.checks().await;
        assert!(find(&checks, name).passed, "{label}: {checks:?}");
        assert_eq!(find(&checks, name).detail, "no upload is open", "{label}");

        let a = after(
            &format!("tb-down-uploads-open-{label}"),
            World {
                bucket: Bucket::ObjectsRefused { uploads: 2 },
                listing,
                ..CLEAN
            },
        )
        .await;
        let checks = a.checks().await;
        assert_eq!(find(&checks, name).detail, "2 uploads are open", "{label}");
    }
}

/// Catches an account that is still known passing as gone: a store that serves its key fails the
/// check and says what it did.
#[tokio::test]
async fn an_account_the_store_still_serves_fails_scoped_account_gone() {
    let a = after(
        "tb-down-account",
        World {
            account_known: true,
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "scoped-account-gone:cold").detail,
        "the account still answers: Reached"
    );
    assert_eq!(
        failing(&checks),
        ["scoped-account-gone:hot", "scoped-account-gone:cold"]
    );
}

/// Catches a verifier that is not there reading as a bucket that is: with no key file named, an
/// unreadable one, or an endpoint the client refuses, the bucket checks fail with that reason.
#[tokio::test]
async fn without_a_usable_verifier_the_bucket_checks_fail_with_the_reason() {
    let a = after("tb-down-no-verifier", CLEAN).await;
    let mut ops = a.ops.clone();
    ops.verifier.remove(&Role::Cold);
    let checks = gather(&a.host, &a.config, &ops, &a.before).await.unwrap();
    for name in ["test-bucket-removed:cold", "no-multipart-uploads:cold"] {
        assert_eq!(
            find(&checks, name).detail,
            "the ops file names no verifier key file"
        );
    }

    a.host
        .files
        .lock()
        .unwrap()
        .remove(Path::new("/private-mark/reader-cold.json"));
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "a file cannot be read: NotFound"
    );

    a.host.file("/private-mark/reader-cold.json", "not json");
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "a key file is not JSON of access_key_id and secret_access_key"
    );

    a.host.file(
        "/private-mark/reader-cold.json",
        r#"{"access_key_id": "READER-KEYID-MARK", "secret_access_key": "READER-SECRET-MARK"}"#,
    );
    let refused = run_config(a.dir.path(), "ftp://host-mark", None);
    let checks = gather(&a.host, &refused, &a.ops, &a.before).await.unwrap();
    assert_eq!(
        find(&checks, "test-bucket-removed:cold").detail,
        "a store's endpoint or bucket name is not accepted"
    );
    assert_eq!(
        find(&checks, "scoped-account-gone:cold").detail,
        "a store's endpoint or bucket name is not accepted"
    );
}

/// Catches a request log that proves nothing passing: a request to a bucket that is not its
/// role's test bucket, a role with no test bucket, a line that does not read and a log that is not
/// there each fail, with counts; blank lines are not requests.
#[tokio::test]
async fn the_request_log_must_name_test_buckets_only_and_read_whole() {
    let a = after("tb-down-log", CLEAN).await;
    let dir = a.dir.path();
    let name = "request-log-test-buckets-only";

    write_log(
        dir,
        &[
            record(Role::Cold, "tb-cold-test-bucket-mark"),
            String::new(),
            record(Role::Cold, "live-cold-bucket-mark"),
        ],
    );
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, name).detail,
        "2 requests: 1 to a bucket that is not the test bucket of its role, 0 unreadable lines"
    );

    write_log(dir, &[record(Role::Proxy, "tb-cold-test-bucket-mark")]);
    assert!(!find(&a.checks().await, name).passed);

    write_log(dir, &["{not a record".to_owned()]);
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, name).detail,
        "1 requests: 0 to a bucket that is not the test bucket of its role, 1 unreadable lines"
    );

    fs::remove_file(dir.join("requests.jsonl")).unwrap();
    let checks = a.checks().await;
    assert!(!find(&checks, name).passed);
}

/// Catches a test instance left on the node passing: a loaded unit, an open port and a
/// directory each fail their own check, and a unit state that cannot be read fails.
#[tokio::test]
async fn what_is_left_of_the_test_instance_fails_its_own_check() {
    let a = after("tb-down-instance", CLEAN).await;
    a.host.open_ports.lock().unwrap().insert(9411);
    a.host.dir("/private-mark/bench-state");
    a.host.answer(|cmd| {
        if cmd.args.last().is_some_and(|w| w == "--value") {
            ok("loaded\n")
        } else if cmd.program.to_str() == Some("systemctl") {
            ok(&show_text(4242, "Thu 2026-10-29 10:00:00 UTC"))
        } else {
            ok("")
        }
    });
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-unit-gone").detail,
        "the unit is still loaded"
    );
    assert_eq!(
        find(&checks, "test-port-free").detail,
        "the port still accepts connections"
    );
    assert_eq!(
        find(&checks, "test-dirs-gone").detail,
        "1 of 2 directories exist"
    );

    a.host.answer(|_| failed(1, ""));
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "test-unit-gone").detail,
        "systemctl show: exited Some(1)"
    );
}

/// Catches a change to the live tierstore going unseen, and a time-varying property raising a
/// false alarm: a different binary, a restarted unit and a changed command line each fail; only
/// a new start time and pid inside `ExecStart` do not.
#[tokio::test]
async fn a_change_to_the_live_tierstore_fails_and_a_clock_tick_does_not() {
    let a = after("tb-down-live", CLEAN).await;
    // The same unit, started again a day later with the same pid in ExecStart's own fields.
    a.host.answer(|cmd| {
        if cmd.args.last().is_some_and(|w| w == "--value") {
            ok("not-found\n")
        } else {
            ok(&show_text(4242, "Fri 2026-10-30 10:00:00 UTC"))
        }
    });
    assert!(find(&a.checks().await, "live-unit-unchanged").passed);

    a.host.answer(|cmd| {
        if cmd.args.last().is_some_and(|w| w == "--value") {
            ok("not-found\n")
        } else {
            ok(&show_text(9999, "Fri 2026-10-30 10:00:00 UTC"))
        }
    });
    let checks = a.checks().await;
    assert!(!find(&checks, "live-unit-unchanged").passed);
    assert!(
        find(&checks, "live-unit-unchanged")
            .detail
            .starts_with("was ")
    );

    a.host.answer(|cmd| {
        if cmd.args.last().is_some_and(|w| w == "--value") {
            ok("not-found\n")
        } else {
            ok(&show_text(4242, "x").replace("--hot-bytes 100", "--hot-bytes 200"))
        }
    });
    assert!(!find(&a.checks().await, "live-unit-unchanged").passed);

    a.host.answer(|_| failed(1, ""));
    assert_eq!(
        find(&a.checks().await, "live-unit-unchanged").detail,
        "systemctl show: exited Some(1)"
    );

    a.host.file("/private-mark/live/tierstore", "a new build");
    let checks = a.checks().await;
    assert!(
        find(&checks, "live-binary-unchanged")
            .detail
            .starts_with("was ")
    );
    a.host
        .files
        .lock()
        .unwrap()
        .remove(Path::new("/private-mark/live/tierstore"));
    assert_eq!(
        find(&a.checks().await, "live-binary-unchanged").detail,
        "cannot read the binary: NotFound"
    );
}

/// Catches a snapshot that does not hold what is compared passing: without the live facts both
/// live checks fail and say which fact is missing; a snapshot that is not there or not JSON stops
/// the verb.
#[tokio::test]
async fn a_snapshot_without_the_facts_fails_and_one_that_is_not_there_stops_the_verb() {
    let a = after("tb-down-snapshot", CLEAN).await;
    fs::write(&a.before, r#"{"facts": {}}"#).unwrap();
    let checks = a.checks().await;
    assert_eq!(
        find(&checks, "live-binary-unchanged").detail,
        "the before-snapshot has no live_binary_sha256"
    );
    assert_eq!(
        find(&checks, "live-unit-unchanged").detail,
        "the before-snapshot has no live_unit_show_sha256"
    );

    fs::write(&a.before, "[]").unwrap();
    let err = gather(&a.host, &a.config, &a.ops, &a.before)
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("before-snapshot: "), "{err}");
    fs::remove_file(&a.before).unwrap();
    let err = gather(&a.host, &a.config, &a.ops, &a.before)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }));
}

/// Catches a verb that exits 0 over a failed check, or that writes nothing: a clean teardown
/// returns the checks and writes them; a bucket left behind ends in an error that names the verb
/// and the checks, after the file is written; nothing private is in the file.
#[tokio::test]
async fn the_verb_writes_the_checks_and_ends_in_an_error_when_one_failed() {
    let a = after("tb-down-verb", CLEAN).await;
    let checks = run_with(&a.host, &a.config, &a.ops, &a.before)
        .await
        .unwrap();
    assert_eq!(checks.len(), 12);
    let text = fs::read_to_string(a.dir.path().join(CHECKS_FILE)).unwrap();
    let back: Vec<Check> = serde_json::from_str(&text).unwrap();
    assert_eq!(back, checks);
    for word in PRIVATE {
        assert!(!text.contains(word), "{word}");
    }

    let a = after(
        "tb-down-verb-failed",
        World {
            bucket: Bucket::Present {
                objects: 1,
                uploads: 0,
            },
            listing: Listing::Has,
            ..CLEAN
        },
    )
    .await;
    let err = run_with(&a.host, &a.config, &a.ops, &a.before)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "verify-teardown: 2 of 12 checks failed: test-bucket-removed:hot, test-bucket-removed:cold"
    );
    assert!(a.dir.path().join(CHECKS_FILE).exists());
}

/// Catches `run` that does not read the ops file of the result directory, or the snapshot named by
/// the flag.
#[tokio::test]
async fn run_reads_the_ops_file_and_the_snapshot_it_is_given() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-down-run");
    let config = run_config(dir.path(), "http://host-mark:9000", None);
    let before = dir.path().join("before.json");
    let err = run(&config, &before).await.unwrap_err();
    assert!(err.to_string().contains("ops.json"), "{err}");

    fs::write(dir.path().join("ops.json"), ops_json().to_string()).unwrap();
    let err = run(&config, &before).await.unwrap_err();
    assert!(err.to_string().contains("before.json"), "{err}");
}

/// Catches a bucket proved removed by a list that contradicts the bucket itself: the account
/// listing does not name the bucket, and the bucket answers with objects. One of the two is
/// wrong, and the check must not pass on the one that says "removed". With no objects (or
/// none read) the plain rule stands.
#[tokio::test]
async fn a_bucket_that_is_not_listed_but_holds_objects_is_a_contradiction() {
    let name = "test-bucket-removed:cold";
    let a = after(
        "tb-down-contradiction",
        World {
            bucket: Bucket::Present {
                objects: 2,
                uploads: 0,
            },
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    let check = find(&checks, name);
    assert!(!check.passed, "{check:?}");
    assert_eq!(
        check.detail,
        "the bucket is not listed but answers with objects: 2 objects"
    );

    let a = after(
        "tb-down-no-contradiction",
        World {
            bucket: Bucket::Present {
                objects: 0,
                uploads: 0,
            },
            ..CLEAN
        },
    )
    .await;
    let checks = a.checks().await;
    assert!(find(&checks, name).passed, "{checks:?}");
}

/// Catches the live-bucket guard reading a second list: the list that did not name the test
/// bucket is the one that must name the live bucket. A store that answers every other list
/// filtered (no live bucket) and the rest full fails the role that got a filtered one, and the
/// verifier asks for the list once per role (two in all).
#[tokio::test]
async fn the_list_that_decided_not_named_is_the_list_that_must_show_the_live_bucket() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inner = store(CLEAN);
    let stub = Stub::start(move |seen| {
        if seen.path == "/" && seen.key_id() == "READER-KEYID-MARK" {
            if calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .is_multiple_of(2)
            {
                named(&[])
            } else {
                bucket_list(false)
            }
        } else {
            inner(seen)
        }
    })
    .await;
    let a = after_on("tb-down-one-list", stub).await;
    let checks = a.checks().await;
    let failed: Vec<&Check> = checks
        .iter()
        .filter(|c| c.name.starts_with("test-bucket-removed:") && !c.passed)
        .collect();
    assert_eq!(failed.len(), 1, "{checks:?}");
    assert!(
        failed[0]
            .detail
            .starts_with("the verifier's bucket list does not show the live bucket live-"),
        "{:?}",
        failed[0]
    );
    let lists = a
        .stub()
        .requests()
        .iter()
        .filter(|r| r.path == "/" && r.key_id() == "READER-KEYID-MARK")
        .count();
    assert_eq!(lists, 2, "one list per role");
}
