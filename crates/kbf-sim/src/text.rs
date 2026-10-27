//! The text a trace spells the scheduler's vocabulary in.
//!
//! Every function here writes its value field by field, in a format fixed in this
//! file. Nothing goes through `Debug`, so renaming a type, a field or a variant in
//! `kbf-types` (or moving it to another crate) cannot change a trace line, and so
//! cannot change a golden hash. A change to the text itself is a change to
//! [`FORMAT_VERSION`](crate::FORMAT_VERSION)'s promise and moves every golden hash that
//! contains the line; see [`crate::golden`].
//!
//! The shapes, with `<n>` a decimal number and `<s>` a quoted, escaped string:
//!
//! | value | text |
//! |---|---|
//! | digest | `sha256:<64 hex>:<size>` |
//! | lease | `<term>.<seq>` |
//! | operation | `op<n>` |
//! | waiter | `w<n>` |
//! | resources | `res(cpu=<n> mem=<n> gpus=<n> vms=<n> leases=<n or ->)` |
//! | outcome | `done(<digest>)` or `failed(<failure>)` |
//! | control record | `lease(<lease> <op> <worker>)`, `result(<lease> <op> <outcome>)`, `refusal(<op> <s>)` |
//!
//! Lists are `[a; b; c]`. An absent optional value is `-`.

use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, Effect, Failure, FarmFault, FarmRun, FarmTime,
    FencePolicy, LeaseId, LeaseKind, LostWhy, MemoryKill, MemoryRun, OperationId, Outcome, Refusal,
    Resources, StartLease, WaiterId, Waiting, WorkKey, WorkerId,
};

/// `text` in double quotes, with backslash, quote and control characters escaped as
/// `str::escape_default` does.
#[must_use]
pub fn quoted(text: &str) -> String {
    format!("\"{}\"", text.escape_default())
}

/// `[a; b; c]`, each item written by `each`.
#[must_use]
pub fn list<T>(items: &[T], each: impl Fn(&T) -> String) -> String {
    let parts: Vec<String> = items.iter().map(each).collect();
    format!("[{}]", parts.join("; "))
}

/// The value written by `each`, or `-` when there is none.
#[must_use]
pub fn optional<T>(value: Option<&T>, each: impl Fn(&T) -> String) -> String {
    value.map_or_else(|| "-".to_owned(), each)
}

/// A farm time: its milliseconds.
#[must_use]
pub fn time(at: FarmTime) -> String {
    at.as_millis().to_string()
}

/// A content digest: `sha256:<64 hex>:<size>`.
#[must_use]
pub fn digest(digest: &Digest) -> String {
    // Indexed by the function's discriminant. A second hash function is a new entry
    // here: until it is added, spelling a digest of it panics, which a test notices,
    // instead of printing it under a wrong name.
    const FUNCTIONS: [&str; 1] = ["sha256"];
    format!(
        "{}:{}:{}",
        FUNCTIONS[digest.function as usize],
        digest.hash_hex(),
        digest.size_bytes
    )
}

/// The dedup text of a work key, spelled as the build spells it: the digest, if the
/// text is one (`sha256:<64 hex>:<size>`), else `other:` and the quoted text.
#[must_use]
pub fn key_action(key: &WorkKey) -> String {
    ActionKey::try_from(key).map_or_else(
        |_| format!("other:{}", quoted(&key.dedup)),
        |action| digest(&action.action),
    )
}

/// A lease id: `<term>.<seq>`.
#[must_use]
pub fn lease(lease: LeaseId) -> String {
    format!("{}.{}", lease.term, lease.seq)
}

/// An operation id: `op<n>`.
#[must_use]
pub fn operation(operation: OperationId) -> String {
    format!("op{}", operation.0)
}

/// A waiter id: `w<n>`.
#[must_use]
pub fn waiter(waiter: WaiterId) -> String {
    format!("w{}", waiter.0)
}

/// A worker's name, as it is.
#[must_use]
pub fn worker(worker: &WorkerId) -> String {
    worker.as_str().to_owned()
}

/// Resources, every axis named.
#[must_use]
pub fn resources(r: &Resources) -> String {
    format!(
        "res(cpu={} mem={} gpus={} vms={} leases={})",
        r.cpu_millis,
        r.memory_bytes,
        r.gpus,
        r.vms,
        r.leases.map_or_else(|| "-".to_owned(), |n| n.to_string())
    )
}

/// A lease kind: `action`, `whole_machine` or `vm`.
#[must_use]
pub fn kind(kind: LeaseKind) -> &'static str {
    match kind {
        LeaseKind::Action => "action",
        LeaseKind::WholeMachine => "whole_machine",
        LeaseKind::Vm => "vm",
    }
}

/// A fence policy: `run_on` or `self_fence`.
#[must_use]
pub fn fence(fence: FencePolicy) -> &'static str {
    match fence {
        FencePolicy::RunOn => "run_on",
        FencePolicy::SelfFence => "self_fence",
    }
}

fn lost_why(why: LostWhy) -> &'static str {
    match why {
        LostWhy::DaemonRestarted => "daemon_restarted",
        LostWhy::Fenced => "fenced",
    }
}

/// A failure: `timeout`, `infra`, `invalid`, `out_of_memory`, `node_memory_pressure`,
/// `missing_input` or `lost(daemon_restarted)` / `lost(fenced)`.
#[must_use]
pub fn failure(failure: Failure) -> String {
    match failure {
        Failure::Timeout => "timeout".to_owned(),
        Failure::Infra => "infra".to_owned(),
        Failure::Invalid => "invalid".to_owned(),
        Failure::OutOfMemory => "out_of_memory".to_owned(),
        Failure::NodeMemoryPressure => "node_memory_pressure".to_owned(),
        Failure::MissingInput => "missing_input".to_owned(),
        Failure::Lost(why) => format!("lost({})", lost_why(why)),
    }
}

/// An outcome: `done(<digest>)` or `failed(<failure>)`.
#[must_use]
pub fn outcome(outcome: Outcome) -> String {
    match outcome {
        Outcome::Completed { action_result } => format!("done({})", digest(&action_result)),
        Outcome::Failed(f) => format!("failed({})", failure(f)),
    }
}

fn farm_fault(fault: FarmFault) -> String {
    match fault {
        FarmFault::NodeMemoryPressure => "node_memory_pressure".to_owned(),
        FarmFault::Lost(why) => format!("lost({})", lost_why(why)),
        FarmFault::Infra => "infra".to_owned(),
    }
}

fn memory_run(run: &MemoryRun) -> String {
    let kill = match run.kill {
        MemoryKill::OwnLimit { cap } => format!("own_limit({cap})"),
        MemoryKill::NodePressure => "node_pressure".to_owned(),
    };
    format!("{}:{}:{kill}", run.worker, run.booked)
}

fn farm_run(run: &FarmRun) -> String {
    format!("{}:{}", run.worker, farm_fault(run.fault))
}

/// A control record. A record kind this function does not know prints as
/// `unknown-record` (the type is non-exhaustive, so a new kind must be added here).
#[must_use]
pub fn record(record: &ControlRecord) -> String {
    match record {
        ControlRecord::Lease(g) => format!(
            "lease({} {} {})",
            lease(g.lease),
            operation(g.operation),
            g.worker
        ),
        ControlRecord::Result(r) => format!(
            "result({} {} {})",
            lease(r.lease),
            operation(r.operation),
            outcome(r.outcome)
        ),
        ControlRecord::Refusal(r) => {
            format!("refusal({} {})", operation(r.operation), quoted(&r.reason))
        }
        _ => "unknown-record".to_owned(),
    }
}

/// A `Start` for a lease.
#[must_use]
pub fn start(start: &StartLease) -> String {
    format!(
        "start(worker={} lease={} op={} instance={} action={} kind={} {} fence={} device={})",
        start.worker,
        lease(start.lease),
        operation(start.operation),
        quoted(&start.key.class),
        key_action(&start.key),
        kind(start.kind),
        resources(&start.resources),
        fence(start.fence),
        optional(start.device.as_ref(), |d| quoted(d)),
    )
}

/// An answer to the waiters of a finished operation.
#[must_use]
pub fn answer(answer: &Answer) -> String {
    format!(
        "answer(op={} lease={} waiters={} outcome={} memory_runs={} farm_runs={})",
        operation(answer.operation),
        lease(answer.lease),
        list(&answer.waiters, |w| waiter(*w)),
        outcome(answer.outcome),
        list(&answer.memory_runs, memory_run),
        list(&answer.farm_runs, farm_run),
    )
}

/// Why an operation waits.
#[must_use]
pub fn waiting(waiting: &Waiting) -> String {
    format!(
        "waiting(op={} reason={})",
        operation(waiting.operation),
        optional(waiting.reason.as_ref(), |r| quoted(r))
    )
}

/// A refusal to the waiters of an operation.
#[must_use]
pub fn refusal(refusal: &Refusal) -> String {
    format!(
        "refuse(op={} waiters={} reason={})",
        operation(refusal.operation),
        list(&refusal.waiters, |w| waiter(*w)),
        quoted(&refusal.reason)
    )
}

/// One effect: `commit <record>`, `<start>`, `<answer>`, `<waiting>` or `<refuse>`.
#[must_use]
pub fn effect(effect: &Effect) -> String {
    match effect {
        Effect::Commit(r) => format!("commit {}", record(r)),
        Effect::Start(s) => start(s),
        Effect::Answer(a) => answer(a),
        Effect::Waiting(w) => waiting(w),
        Effect::Refuse(r) => refusal(r),
    }
}

/// Effects, in order, as a list.
#[must_use]
pub fn effects(effects: &[Effect]) -> String {
    list(effects, effect)
}

#[cfg(test)]
mod tests {
    use kbf_types::{DigestFunction, LeaseGrant, RefusalRecord, ResultRecord};

    use super::*;

    fn d(n: u8) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], 7)
    }

    fn w(name: &str) -> WorkerId {
        WorkerId::new(name)
    }

    /// Catches: a work key spelled differently from the digest it was made from (every
    /// pinned trace line that names an action would move), and one that is not a digest
    /// printed as if it were.
    #[test]
    fn a_key_spells_its_digest_or_says_it_is_not_one() {
        let key = crate::work_key("main", d(0xab));
        assert_eq!(key_action(&key), digest(&d(0xab)));
        assert_eq!(
            key_action(&WorkKey::new("main", "job \"7\"")),
            "other:\"job \\\"7\\\"\""
        );
    }

    fn lease_id() -> LeaseId {
        LeaseId { term: 2, seq: 9 }
    }

    /// Catches: a field of a leaf value dropped, reordered or renamed in the text; the
    /// expected strings are the format, not a recomputation.
    #[test]
    fn leaves_are_spelled_field_by_field() {
        assert_eq!(digest(&d(0xab)), format!("sha256:{}:7", "ab".repeat(32)),);
        assert_eq!(lease(lease_id()), "2.9");
        assert_eq!(operation(OperationId(4)), "op4");
        assert_eq!(waiter(WaiterId(5)), "w5");
        assert_eq!(worker(&w("node-1")), "node-1");
        assert_eq!(time(FarmTime::from_millis(1_500)), "1500");
        assert_eq!(
            resources(&Resources::new(1_000, 2_048)),
            "res(cpu=1000 mem=2048 gpus=0 vms=0 leases=-)"
        );
        assert_eq!(
            resources(
                &Resources::new(1, 2)
                    .with_gpus(3)
                    .with_vms(4)
                    .with_lease_cap(5)
            ),
            "res(cpu=1 mem=2 gpus=3 vms=4 leases=5)"
        );
    }

    /// Catches: two variants of one enum sharing a spelling (two runs that differ only
    /// there would share a hash), or a variant renamed.
    #[test]
    fn enum_variants_each_have_their_own_spelling() {
        let kinds: Vec<&str> = LeaseKind::ALL.into_iter().map(kind).collect();
        assert_eq!(kinds, ["action", "whole_machine", "vm"]);
        assert_eq!(fence(FencePolicy::RunOn), "run_on");
        assert_eq!(fence(FencePolicy::SelfFence), "self_fence");
        let failures = [
            Failure::Timeout,
            Failure::Infra,
            Failure::Invalid,
            Failure::OutOfMemory,
            Failure::NodeMemoryPressure,
            Failure::MissingInput,
            Failure::Lost(LostWhy::DaemonRestarted),
            Failure::Lost(LostWhy::Fenced),
        ]
        .map(failure);
        assert_eq!(
            failures,
            [
                "timeout",
                "infra",
                "invalid",
                "out_of_memory",
                "node_memory_pressure",
                "missing_input",
                "lost(daemon_restarted)",
                "lost(fenced)",
            ]
        );
        assert_eq!(
            outcome(Outcome::Completed {
                action_result: d(1)
            }),
            format!("done(sha256:{}:7)", "01".repeat(32))
        );
        assert_eq!(outcome(Outcome::Failed(Failure::Infra)), "failed(infra)");
    }

    /// Catches: quoting that lets a quote or a newline in a reason break a line in two
    /// (one trace line must stay one line).
    #[test]
    fn quoting_keeps_one_value_on_one_line() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted("a \"b\"\nc"), "\"a \\\"b\\\"\\nc\"");
        assert_eq!(list(&[1, 2], |n| n.to_string()), "[1; 2]");
        assert_eq!(list::<u8>(&[], |n| n.to_string()), "[]");
        assert_eq!(optional(Some(&3), |n| n.to_string()), "3");
        assert_eq!(optional::<u8>(None, |n| n.to_string()), "-");
    }

    /// Catches: a record or effect kind missing from the text, or its fields in another
    /// order than the format promises.
    #[test]
    fn records_and_effects_are_spelled_out() {
        let grant = ControlRecord::Lease(LeaseGrant {
            lease: lease_id(),
            operation: OperationId(3),
            worker: w("a"),
        });
        let result = ControlRecord::Result(ResultRecord {
            lease: lease_id(),
            operation: OperationId(3),
            outcome: Outcome::Failed(Failure::Timeout),
        });
        let refused = ControlRecord::Refusal(RefusalRecord {
            operation: OperationId(3),
            reason: "too big".to_owned(),
        });
        assert_eq!(record(&grant), "lease(2.9 op3 a)");
        assert_eq!(record(&result), "result(2.9 op3 failed(timeout))");
        assert_eq!(record(&refused), "refusal(op3 \"too big\")");

        let key = crate::work_key("main", d(2));
        let started = StartLease {
            worker: w("a"),
            lease: lease_id(),
            operation: OperationId(3),
            key,
            kind: LeaseKind::Vm,
            resources: Resources::new(1, 2),
            fence: FencePolicy::SelfFence,
            device: Some("ios-1".to_owned()),
        };
        let expected_start = format!(
            "start(worker=a lease=2.9 op=op3 instance=\"main\" action=sha256:{}:7 kind=vm \
             res(cpu=1 mem=2 gpus=0 vms=0 leases=-) fence=self_fence device=\"ios-1\")",
            "02".repeat(32)
        );
        assert_eq!(start(&started), expected_start);
        let no_device = StartLease {
            device: None,
            ..started.clone()
        };
        assert!(start(&no_device).ends_with("device=-)"));

        let answered = Answer {
            operation: OperationId(3),
            lease: lease_id(),
            waiters: vec![WaiterId(1), WaiterId(2)],
            outcome: Outcome::Failed(Failure::OutOfMemory),
            memory_runs: vec![
                MemoryRun {
                    worker: w("a"),
                    booked: 10,
                    kill: MemoryKill::OwnLimit { cap: 8 },
                },
                MemoryRun {
                    worker: w("b"),
                    booked: 11,
                    kill: MemoryKill::NodePressure,
                },
            ],
            farm_runs: vec![
                FarmRun {
                    worker: w("a"),
                    fault: FarmFault::NodeMemoryPressure,
                },
                FarmRun {
                    worker: w("b"),
                    fault: FarmFault::Lost(LostWhy::Fenced),
                },
                FarmRun {
                    worker: w("c"),
                    fault: FarmFault::Infra,
                },
            ],
        };
        assert_eq!(
            answer(&answered),
            "answer(op=op3 lease=2.9 waiters=[w1; w2] outcome=failed(out_of_memory) \
             memory_runs=[a:10:own_limit(8); b:11:node_pressure] \
             farm_runs=[a:node_memory_pressure; b:lost(fenced); c:infra])"
        );
        let waits = Waiting {
            operation: OperationId(3),
            reason: None,
        };
        assert_eq!(waiting(&waits), "waiting(op=op3 reason=-)");
        let refuse = Refusal {
            operation: OperationId(3),
            waiters: vec![WaiterId(1)],
            reason: "no".to_owned(),
        };
        assert_eq!(
            refusal(&refuse),
            "refuse(op=op3 waiters=[w1] reason=\"no\")"
        );

        let all = [
            Effect::Commit(grant),
            Effect::Start(started),
            Effect::Answer(answered),
            Effect::Waiting(waits),
            Effect::Refuse(refuse),
        ];
        let text = effects(&all);
        assert!(
            text.starts_with("[commit lease(2.9 op3 a); start(worker=a"),
            "{text}"
        );
        assert!(
            text.ends_with("refuse(op=op3 waiters=[w1] reason=\"no\")]"),
            "{text}"
        );
    }
}
