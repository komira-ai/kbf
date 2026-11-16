//! The `exec/` rows over the in-memory store, whose clock a test moves by hand.

use std::sync::Arc;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Revision};

use super::*;

const TTL: Duration = Duration::from_secs(45);

struct Rig {
    clock: Arc<ManualClock>,
    fake: Arc<Fake>,
    rows: ExecRows,
}

fn rig() -> Rig {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let rows = ExecRows::new(Arc::clone(&fake) as Arc<dyn Store>);
    Rig { clock, fake, rows }
}

impl Rig {
    fn put(&self, key: &str, value: Vec<u8>) {
        let put = Txn::new().put(key, value, Cond::Any);
        self.fake.txn(put).expect("store").expect("no conflict");
    }

    fn expire(&self) {
        self.clock.advance(TTL + Duration::from_secs(1));
    }

    fn owner(&self, worker: &str) -> String {
        let entries = self.rows.workers().expect("workers");
        let entry = entries.iter().find(|e| e.worker == worker);
        entry.expect("a worker row").row.owner_instance.clone()
    }

    fn entry(&self, worker: &str) -> WorkerEntry {
        let entries = self.rows.workers().expect("workers");
        entries
            .into_iter()
            .find(|e| e.worker == worker)
            .expect("a worker row")
    }
}

fn alive() -> Liveness {
    Liveness {
        group: "kbf".to_owned(),
        started_ms: 1_700_000_000_000,
        release: "0.1.0".to_owned(),
        draining: false,
    }
}

fn shape() -> Shape {
    Shape {
        cpu_millis: 8_000,
        memory_bytes: 17_179_869_184,
        slots: 4,
        arch: "x86_64".to_owned(),
    }
}

fn worker_row(owner: &str) -> WorkerRow {
    WorkerRow {
        owner_instance: owner.to_owned(),
        farm_instance: format!("farm-{owner}"),
        shape: shape(),
    }
}

fn job_row(seq: u64) -> JobRow {
    JobRow {
        lease_term: 7,
        lease_seq: seq,
        action: "sha256:ab/12".to_owned(),
        pack_key: "cas/b/build/nonce-0".to_owned(),
        submitted_ms: 1_700_000_000_500,
    }
}

/// `owner` beats and creates `worker`: the state of a frontend that registered a worker.
fn started(rig: &Rig, owner: &str, worker: &str) -> (Live, Held) {
    let live = rig.rows.beat(owner, &alive(), TTL, &[]).expect("beat");
    let held = rig
        .rows
        .create_worker(worker, &worker_row(owner))
        .expect("create");
    (live, held)
}

fn conflict(result: Result<impl std::fmt::Debug, RowsError>, key: &str) {
    let error = result.expect_err("a conflict");
    assert!(
        error.conflicts_on(key),
        "{error:?} is not a conflict on {key}"
    );
}

/// The rows are a format: the bytes of each are what a build wrote, so a change to a key,
/// the version byte or a field name fails here and a later build reads this one's rows.
#[test]
fn each_row_has_the_bytes_of_its_format() {
    let rig = rig();
    rig.rows.beat("i1", &alive(), TTL, &[]).expect("beat");
    rig.rows
        .create_worker("js:kbf.i1-0", &worker_row("i1"))
        .expect("create");
    let held = Held {
        worker: "js:kbf.i1-0".to_owned(),
        version: rig.entry("js:kbf.i1-0").version,
    };
    rig.rows
        .insert_job(&held, "k1-2-0", &job_row(9))
        .expect("insert");

    let value = |key: &str| rig.fake.get(key).expect("get").expect("a row").value;
    let text = |key: &str| {
        let bytes = value(key);
        assert_eq!(bytes[0], 1, "the version byte of {key}");
        String::from_utf8(bytes[1..].to_vec()).expect("utf-8")
    };
    assert_eq!(
        text("exec/fe/i1"),
        r#"{"draining":false,"group":"kbf","release":"0.1.0","started_ms":1700000000000}"#
    );
    assert_eq!(
        text("exec/w/js:kbf.i1-0"),
        concat!(
            r#"{"farm_instance":"farm-i1","owner_instance":"i1","#,
            r#""shape":{"arch":"x86_64","cpu_millis":8000,"memory_bytes":17179869184,"slots":4}}"#
        )
    );
    assert_eq!(
        text("exec/j/js:kbf.i1-0/k1-2-0"),
        concat!(
            r#"{"action":"sha256:ab/12","lease_seq":9,"lease_term":7,"#,
            r#""pack_key":"cas/b/build/nonce-0","submitted_ms":1700000000500}"#
        )
    );
}

#[test]
fn rows_read_back_as_written() {
    let rig = rig();
    let (_, held) = started(&rig, "i1", "w1");
    rig.rows
        .insert_job(&held, "j1", &job_row(1))
        .expect("insert");
    rig.rows
        .insert_job(&held, "j2", &job_row(2))
        .expect("insert");

    assert_eq!(rig.rows.liveness("i1").expect("read"), Some(alive()));
    assert_eq!(rig.rows.liveness("i2").expect("read"), None);
    assert_eq!(
        rig.rows.live_instances().expect("scan"),
        vec![("i1".to_owned(), alive())]
    );
    let entry = rig.entry("w1");
    assert_eq!(
        (entry.row.clone(), entry.version),
        (worker_row("i1"), held.version)
    );
    let jobs = rig.rows.jobs("w1").expect("jobs");
    let seen: Vec<_> = jobs
        .iter()
        .map(|j| (j.job.as_str(), j.row.lease_seq))
        .collect();
    assert_eq!(seen, [("j1", 1), ("j2", 2)]);
}

/// Catches a claim that does not wait for the owner's liveness row to go: it must refuse
/// while the row is there and succeed on the store's clock once it expired.
#[test]
fn a_claim_needs_the_owners_liveness_row_absent() {
    let rig = rig();
    started(&rig, "a", "w");
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");

    let entry = rig.entry("w");
    conflict(rig.rows.claim(&b, &entry, "farm-b"), "exec/fe/a");
    assert_eq!(rig.owner("w"), "a");

    rig.expire();
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    let held = rig.rows.claim(&b, &entry, "farm-b").expect("claim");
    assert_eq!(rig.owner("w"), "b");
    let claimed = rig.entry("w");
    assert_eq!(claimed.version, held.version);
    assert_eq!(claimed.row.farm_instance, "farm-b");
    assert_eq!(claimed.row.shape, shape());
}

#[test]
fn a_released_worker_is_claimed_at_once_and_an_own_worker_again() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    rig.rows.release(&held, &worker_row("a")).expect("release");
    assert_eq!(rig.owner("w"), "");
    let taken = rig
        .rows
        .claim(&b, &rig.entry("w"), "farm-b")
        .expect("claim");

    // The owner dropped it by self-fencing and still beats: it claims its own row again.
    let a = rig.rows.beat("b", &alive(), TTL, &[taken]).expect("beat");
    let again = rig
        .rows
        .claim(&a, &rig.entry("w"), "farm-b2")
        .expect("claim");
    assert_eq!(rig.entry("w").version, again.version);
    assert_eq!(rig.owner("w"), "b");
}

#[test]
fn two_claims_of_one_worker_have_one_winner() {
    let rig = rig();
    started(&rig, "a", "w");
    rig.expire();
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    let c = rig.rows.beat("c", &alive(), TTL, &[]).expect("beat");
    let seen = rig.entry("w");

    let won = rig.rows.claim(&b, &seen, "farm-b").expect("one wins");
    conflict(rig.rows.claim(&c, &seen, "farm-c"), "exec/w/w");
    assert_eq!(rig.owner("w"), "b");
    assert_eq!(rig.entry("w").version, won.version);
}

#[test]
fn a_process_whose_liveness_row_expired_claims_nothing() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    rig.rows.release(&held, &worker_row("a")).expect("release");
    rig.expire();
    conflict(rig.rows.claim(&b, &rig.entry("w"), "farm-b"), "exec/fe/b");
    assert_eq!(rig.owner("w"), "");

    // A beat after the expiry is a new row: the old token no longer matches it.
    rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    conflict(rig.rows.claim(&b, &rig.entry("w"), "farm-b"), "exec/fe/b");
}

/// Every write of a holder carries a check of the worker row: after another process claimed,
/// the job insert, the job delete, the beat, the release and the retirement of the loser all
/// fail and write nothing.
#[test]
fn a_job_write_without_the_worker_version_is_refused() {
    let rig = rig();
    let (_, old) = started(&rig, "a", "w");
    rig.rows
        .insert_job(&old, "j1", &job_row(1))
        .expect("insert");
    rig.expire();
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    rig.rows
        .claim(&b, &rig.entry("w"), "farm-b")
        .expect("claim");

    conflict(rig.rows.insert_job(&old, "j2", &job_row(2)), "exec/w/w");
    conflict(rig.rows.delete_job(&old, "j1"), "exec/w/w");
    conflict(
        rig.rows
            .beat("a", &alive(), TTL, std::slice::from_ref(&old)),
        "exec/w/w",
    );
    conflict(rig.rows.release(&old, &worker_row("a")), "exec/w/w");
    conflict(rig.rows.retire(&old), "exec/w/w");

    assert_eq!(
        rig.rows.liveness("a").expect("read"),
        None,
        "the failed beat renewed"
    );
    assert_eq!(rig.owner("w"), "b");
    let jobs = rig.rows.jobs("w").expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job, "j1");
}

#[test]
fn a_job_row_is_inserted_once_and_a_repeat_goes_on() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    assert_eq!(
        rig.rows
            .insert_job(&held, "j", &job_row(1))
            .expect("insert"),
        Insert::Written
    );
    assert_eq!(
        rig.rows
            .insert_job(&held, "j", &job_row(2))
            .expect("insert"),
        Insert::Present
    );
    let jobs = rig.rows.jobs("w").expect("jobs");
    assert_eq!(jobs[0].row, job_row(1), "the repeat replaced the row");

    // A repeat by a process that lost the worker meanwhile does not go on.
    rig.expire();
    let b = rig.rows.beat("b", &alive(), TTL, &[]).expect("beat");
    rig.rows
        .claim(&b, &rig.entry("w"), "farm-b")
        .expect("claim");
    conflict(rig.rows.insert_job(&held, "j", &job_row(1)), "exec/w/w");
}

#[test]
fn a_job_row_goes_after_the_ack_and_a_repeat_changes_nothing() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    rig.rows
        .insert_job(&held, "j", &job_row(1))
        .expect("insert");
    rig.rows.delete_job(&held, "j").expect("delete");
    rig.rows.delete_job(&held, "j").expect("delete again");
    assert!(rig.rows.jobs("w").expect("jobs").is_empty());
    rig.rows.retire(&held).expect("retire");
    assert!(rig.rows.workers().expect("workers").is_empty());
}

#[test]
fn a_beat_renews_only_while_every_worker_is_held() {
    let rig = rig();
    let (_, w1) = started(&rig, "a", "w1");
    let w2 = rig
        .rows
        .create_worker("w2", &worker_row("a"))
        .expect("create");
    let live = rig
        .rows
        .beat("a", &alive(), TTL, &[w1.clone(), w2.clone()])
        .expect("beat");
    assert_eq!(live.instance, "a");

    rig.clock.advance(TTL / 2);
    rig.rows.beat("a", &alive(), TTL, &[w1, w2]).expect("beat");
    rig.clock.advance(TTL / 2 + Duration::from_secs(1));
    assert!(
        rig.rows.liveness("a").expect("read").is_some(),
        "the renewal did not extend the row"
    );

    rig.rows.leave("a").expect("leave");
    assert_eq!(rig.rows.liveness("a").expect("read"), None);
    rig.rows.leave("a").expect("leave again");
}

#[test]
fn jobs_of_one_worker_are_not_those_of_a_worker_it_prefixes() {
    let rig = rig();
    let (_, w) = started(&rig, "a", "w");
    let wx = rig
        .rows
        .create_worker("wx", &worker_row("a"))
        .expect("create");
    rig.rows.insert_job(&w, "j", &job_row(1)).expect("insert");
    rig.rows.insert_job(&wx, "k", &job_row(2)).expect("insert");
    let names = |worker: &str| -> Vec<String> {
        rig.rows
            .jobs(worker)
            .expect("jobs")
            .into_iter()
            .map(|j| j.job)
            .collect()
    };
    assert_eq!(names("w"), ["j"]);
    assert_eq!(names("wx"), ["k"]);
}

#[test]
fn a_name_that_is_not_a_key_part_is_refused_everywhere() {
    let rig = rig();
    let bad = |result: Result<(), RowsError>, name: &str| {
        assert_eq!(
            result,
            Err(RowsError::BadName {
                name: name.to_owned()
            })
        );
    };
    let held = |worker: &str| Held {
        worker: worker.to_owned(),
        version: Version(1),
    };
    let live = |instance: &str| Live {
        instance: instance.to_owned(),
        version: Version(1),
    };
    let entry = |worker: &str, owner: &str| WorkerEntry {
        worker: worker.to_owned(),
        row: worker_row(owner),
        version: Version(1),
    };
    for name in ["", "a/b"] {
        bad(rig.rows.beat(name, &alive(), TTL, &[]).map(drop), name);
        bad(
            rig.rows.beat("a", &alive(), TTL, &[held(name)]).map(drop),
            name,
        );
        bad(rig.rows.leave(name), name);
        bad(rig.rows.liveness(name).map(drop), name);
        bad(
            rig.rows.create_worker(name, &worker_row("a")).map(drop),
            name,
        );
        bad(
            rig.rows.claim(&live(name), &entry("w", "a"), "f").map(drop),
            name,
        );
        bad(
            rig.rows.claim(&live("b"), &entry(name, "a"), "f").map(drop),
            name,
        );
        if !name.is_empty() {
            // An empty owner is a released row, which names no liveness row to check.
            bad(
                rig.rows.claim(&live("b"), &entry("w", name), "f").map(drop),
                name,
            );
        }
        bad(rig.rows.release(&held(name), &worker_row("a")), name);
        bad(rig.rows.retire(&held(name)), name);
        bad(
            rig.rows.insert_job(&held(name), "j", &job_row(1)).map(drop),
            name,
        );
        bad(
            rig.rows.insert_job(&held("w"), name, &job_row(1)).map(drop),
            name,
        );
        bad(rig.rows.delete_job(&held(name), "j"), name);
        bad(rig.rows.delete_job(&held("w"), name), name);
        bad(rig.rows.jobs(name).map(drop), name);
    }
    assert_eq!(job_key("w", "j").as_deref(), Ok("exec/j/w/j"));
}

/// A row a later release wrote is not one this build reads: a scan skips it and counts it, a
/// single read finds none, and the rows beside it read as before.
#[test]
fn an_unknown_row_version_is_skipped_and_counted() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    rig.rows
        .insert_job(&held, "j1", &job_row(1))
        .expect("insert");
    rig.put("exec/fe/z", vec![2, b'{', b'}']);
    rig.put("exec/w/z", vec![2, b'x']);
    rig.put("exec/j/w/j2", vec![0]);
    assert_eq!(rig.rows.unknown_rows(), 0);

    assert_eq!(
        rig.rows.live_instances().expect("scan"),
        vec![("a".to_owned(), alive())]
    );
    assert_eq!(rig.rows.unknown_rows(), 1);
    let workers = rig.rows.workers().expect("scan");
    assert_eq!(workers.len(), 1);
    assert_eq!(rig.rows.unknown_rows(), 2);
    assert_eq!(rig.rows.jobs("w").expect("scan").len(), 1);
    assert_eq!(rig.rows.unknown_rows(), 3);
    assert_eq!(rig.rows.liveness("z").expect("read"), None);
    assert_eq!(rig.rows.unknown_rows(), 4);
}

/// A field a later release adds is ignored: the row reads as the row without it.
#[test]
fn a_field_a_later_release_adds_is_ignored() {
    let rig = rig();
    let live = br#"{"draining":true,"group":"g","new":[1],"release":"r","started_ms":5}"#;
    let work = br#"{"farm_instance":"f","next":1,"owner_instance":"o","shape":{"arch":"a","cpu_millis":1,"extra":0,"memory_bytes":2,"slots":3}}"#;
    let job =
        br#"{"action":"d","lease_seq":2,"lease_term":1,"more":{},"pack_key":"k","submitted_ms":4}"#;
    let versioned = |body: &[u8]| [&[VERSION][..], body].concat();
    rig.put("exec/fe/i", versioned(live));
    rig.put("exec/w/w", versioned(work));
    rig.put("exec/j/w/j", versioned(job));

    let expected = Liveness {
        group: "g".to_owned(),
        started_ms: 5,
        release: "r".to_owned(),
        draining: true,
    };
    assert_eq!(rig.rows.liveness("i").expect("read"), Some(expected));
    let entry = rig.entry("w");
    assert_eq!(
        entry.row.shape,
        Shape {
            cpu_millis: 1,
            memory_bytes: 2,
            slots: 3,
            arch: "a".to_owned()
        }
    );
    let jobs = rig.rows.jobs("w").expect("jobs");
    assert_eq!(jobs[0].row.pack_key, "k");
    assert_eq!(rig.rows.unknown_rows(), 0);
}

/// A row of a known version that is not what it says is refused, by its key and the reason,
/// whether it is read alone or in a scan.
#[test]
fn a_corrupt_row_is_refused_with_its_key() {
    let corrupt_key = |result: Result<(), RowsError>, key: &str| match result {
        Err(RowsError::Corrupt { key: k, why }) => {
            assert_eq!(k, key);
            assert!(!why.is_empty());
        }
        other => panic!("expected a corrupt row at {key}, got {other:?}"),
    };

    let rig = rig();
    rig.put("exec/fe/i", vec![VERSION, b'x']);
    corrupt_key(rig.rows.liveness("i").map(drop), "exec/fe/i");
    corrupt_key(rig.rows.live_instances().map(drop), "exec/fe/i");

    rig.put("exec/w/w", vec![VERSION, b'{', b'}']);
    corrupt_key(rig.rows.workers().map(drop), "exec/w/w");

    rig.put("exec/j/w/j", vec![VERSION, b'[', b']']);
    corrupt_key(rig.rows.jobs("w").map(drop), "exec/j/w/j");

    rig.put("exec/j/w/j", Vec::new());
    let result = rig.rows.jobs("w");
    let error = result.expect_err("an empty value");
    assert_eq!(
        error,
        RowsError::Corrupt {
            key: "exec/j/w/j".to_owned(),
            why: "no version byte".to_owned()
        }
    );
    assert!(error.to_string().contains("exec/j/w/j"));
}

#[test]
fn a_failed_store_is_the_stores_error() {
    struct Down;
    impl Store for Down {
        fn apply_batch(&self, _: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
            Err(StoreError::Fenced)
        }
        fn get(&self, _: &str) -> Result<Option<Versioned>, StoreError> {
            Err(StoreError::Fenced)
        }
        fn scan_prefix(&self, _: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
            Err(StoreError::Fenced)
        }
        fn scan_range(
            &self,
            _: kbf_state::ScanRange<'_>,
        ) -> Result<kbf_state::ScanPage, StoreError> {
            Err(StoreError::Fenced)
        }
        fn changes(&self, _: Revision, _: usize) -> Result<kbf_state::Changes, StoreError> {
            Err(StoreError::Fenced)
        }
        fn trim_feed(&self, _: Revision) -> Result<u64, StoreError> {
            Err(StoreError::Fenced)
        }
        fn reap_expired(&self) -> Result<usize, StoreError> {
            Err(StoreError::Fenced)
        }
        fn commit_stats(&self) -> kbf_state::CommitStats {
            kbf_state::CommitStats::default()
        }
        fn now(&self) -> kbf_state::StoreTime {
            kbf_state::StoreTime::ZERO
        }
    }
    let rows = ExecRows::new(Arc::new(Down));
    let fenced = Err(RowsError::Store(StoreError::Fenced));
    assert_eq!(rows.leave("a"), fenced);
    assert_eq!(rows.liveness("a").map(drop), fenced);
    assert_eq!(rows.live_instances().map(drop), fenced);
    assert_eq!(rows.workers().map(drop), fenced);
    assert_eq!(rows.jobs("w").map(drop), fenced);
    let held = Held {
        worker: "w".to_owned(),
        version: Version(1),
    };
    assert_eq!(rows.insert_job(&held, "j", &job_row(1)).map(drop), fenced);
}

/// A repeat of a job insert for a worker whose row is gone is a lost worker, not a row that
/// is there already.
#[test]
fn a_repeat_insert_after_the_worker_was_retired_is_refused() {
    let rig = rig();
    let (_, held) = started(&rig, "a", "w");
    rig.rows
        .insert_job(&held, "j", &job_row(1))
        .expect("insert");
    // The worker row is gone (retired by another process after it claimed and finished).
    rig.rows.retire(&held).expect("retire");
    conflict(rig.rows.insert_job(&held, "j", &job_row(1)), "exec/w/w");
}
