//! The rows only a build server has ([`Ext`]): what the `action usage:` line says an
//! operation is (`subject/`), the name and action of a finished operation (`doneof/`) and the
//! learned-sizes shadow's size summaries (`est/`).
//!
//! The store layer writes the rows every scheduler needs, and calls its `Ext` to add these to
//! the transactions it builds (a submission, the end of an operation) and to read them back
//! (a start, a look at an operation, the size summaries). [`KbfRows`], the one
//! implementation, keeps the formats of the `subject`, `named` and `estimate` modules.

use std::collections::BTreeMap;

use kbf_estimator::{Day, Summaries};

use super::*;

/// A size summary row the sweep changes: its key, its new value (`None`: delete it) and the
/// version it was read at.
pub(super) type Change = (String, Option<Vec<u8>>, Version);

/// The rows of a build server, beside the scheduler's.
pub(super) trait Ext: Send + Sync {
    /// `txn`, and the `subject/` row of the operation a submission queues.
    fn submitted(&self, txn: Txn, operation: OperationId, subject: &Subject) -> Txn;

    /// `txn`, and the removal of the rows `operation` leaves when it ends.
    fn settled(&self, txn: Txn, operation: OperationId) -> Txn;

    /// `txn`, and the put of the row that keeps `action` as long as the result of
    /// `operation`, which ends in `txn`: for `ttl`.
    fn ended(&self, txn: Txn, operation: OperationId, action: &ActionKey, ttl: Duration) -> Txn;

    /// The subject `operation`'s row holds, if it has one.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a subject.
    fn subject(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Subject>, StateError>;

    /// The subject of every operation that has a row.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not a subject.
    fn subjects(&self, store: &dyn Store) -> Result<BTreeMap<OperationId, Subject>, StateError>;

    /// The action of the finished or refused `operation`, if the store kept it.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not an action.
    fn action_of(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<ActionKey>, StateError>;

    /// The size summaries the store holds. A row that cannot be read is skipped.
    ///
    /// # Errors
    /// The store cannot be read.
    fn estimates(&self, store: &dyn Store) -> Result<Vec<Row>, StateError>;

    /// `txn`, and the put of the size summaries `rows`.
    fn put_estimates(&self, txn: Txn, rows: &[Row]) -> Txn;

    /// The size summary rows idle past the estimator's stale days on `today`, to delete, and
    /// those that hold a floor, to strip of their samples.
    ///
    /// # Errors
    /// The store cannot be read.
    fn stale_estimates(&self, store: &dyn Store, today: Day) -> Result<Vec<Change>, StateError>;

    /// The name a `keyfloor/` row gives the size summary `key`.
    fn summary_name(&self, key: &kbf_estimator::Key) -> String;

    /// The size summary a name names, if it names one.
    fn summary_key(&self, name: &str) -> Option<kbf_estimator::Key>;
}

/// The rows of a build server, in the formats of the `subject`, `named` and `estimate` modules.
#[derive(Debug, Clone, Copy)]
pub(super) struct KbfRows;

impl Ext for KbfRows {
    fn submitted(&self, txn: Txn, operation: OperationId, subject: &Subject) -> Txn {
        txn.put(
            subject_key(operation),
            subject::encode(subject),
            Cond::Absent,
        )
    }

    fn settled(&self, txn: Txn, operation: OperationId) -> Txn {
        txn.delete(subject_key(operation), Cond::Any)
    }

    fn ended(&self, txn: Txn, operation: OperationId, action: &ActionKey, ttl: Duration) -> Txn {
        txn.put_ttl(
            named::doneof_key(operation),
            named::encode(action),
            ttl,
            Cond::Any,
        )
    }

    fn subject(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Subject>, StateError> {
        let key = subject_key(operation);
        let row = store.get(&key)?;
        row.map(|row| subject::decode(&row.value).map_err(|why| corrupt(&key, why)))
            .transpose()
    }

    fn subjects(&self, store: &dyn Store) -> Result<BTreeMap<OperationId, Subject>, StateError> {
        let mut subjects = BTreeMap::new();
        for (key, row) in store.scan_prefix(SUBJECT)? {
            let operation = parse_operation(&key, SUBJECT)?;
            let subject = subject::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            subjects.insert(operation, subject);
        }
        Ok(subjects)
    }

    fn action_of(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<ActionKey>, StateError> {
        let key = named::doneof_key(operation);
        let Some(row) = store.get(&key)? else {
            return Ok(None);
        };
        named::decode(&row.value)
            .map(Some)
            .map_err(|why| corrupt(&key, why))
    }

    fn estimates(&self, store: &dyn Store) -> Result<Vec<Row>, StateError> {
        let mut rows = Vec::new();
        for (key, row) in store.scan_prefix(ESTIMATE)? {
            match estimate::decode(&key, &row.value) {
                Some(row) => rows.push(row),
                None => tracing::warn!(%key, "a size summary row cannot be read: skipped"),
            }
        }
        Ok(rows)
    }

    fn put_estimates(&self, mut txn: Txn, rows: &[Row]) -> Txn {
        for (key, summary) in rows {
            txn = txn.put(estimate::row_key(key), estimate::encode(summary), Cond::Any);
        }
        txn
    }

    fn stale_estimates(&self, store: &dyn Store, today: Day) -> Result<Vec<Change>, StateError> {
        let mut changes = Vec::new();
        for (name, row) in store.scan_prefix(ESTIMATE)? {
            let Some((key, summary)) = estimate::decode(&name, &row.value) else {
                continue;
            };
            let mut held = Summaries::default();
            if held.insert_encoded(&key, &summary).is_err() {
                continue;
            }
            held.expire(today);
            match held.get(&key) {
                None => changes.push((name, None, row.version)),
                Some(kept) if kept.encode() != summary => {
                    changes.push((name, Some(estimate::encode(&kept.encode())), row.version));
                }
                Some(_) => {}
            }
        }
        Ok(changes)
    }

    fn summary_name(&self, key: &kbf_estimator::Key) -> String {
        estimate::row_key(key)
    }

    fn summary_key(&self, name: &str) -> Option<kbf_estimator::Key> {
        estimate::parse_key(name)
    }
}
