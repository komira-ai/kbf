//! The rows only a build server has ([`Ext`]): `subject/<operation>`, what the `action usage:`
//! line says the operation is, kept from its submission to its end, and `doneof/<operation>`,
//! the name and action of a finished operation, kept as long as its result.
//!
//! `js-store` sees what the server keeps with an operation only as bytes (a parcel). Here the
//! parcel of a submission is the value of its `subject/` row, as the `subject` module of the
//! store layer encodes it, and the parcel of an ending is the value of its `doneof/` row, as
//! the `named` module encodes it; both modules also read the rows back, so a row that is not
//! one of theirs is refused as the store layer refuses it.
//!
//! The learned-sizes shadow's size summaries (`est/`) are not part of an operation's
//! transactions, so `js-store` has no call for them: [`KbfExt`] has methods of its own for them
//! ([`KbfExt::put_estimates`], [`KbfExt::estimates`] and the row name [`KbfExt::summary_name`]
//! that a key floor gives its summary), in the `estimate` module's format. The sweep of stale
//! summaries is not here yet.

use std::collections::BTreeMap;
use std::time::Duration;

use js_store::{Ext, StateError, keys};
use kbf_estimator::Key;
use kbf_state::{Cond, Store, Txn};
use kbf_types::OperationId;

use crate::farm::durable::{ESTIMATE, SUBJECT, estimate, named, subject};
use crate::learned::Row;

/// The rows of a build server, beside the scheduler's.
#[derive(Debug, Clone, Copy, Default)]
pub struct KbfExt;

impl KbfExt {
    /// The name a `keyfloor/` row gives the size summary `key`: the key of its `est/` row.
    #[must_use]
    pub fn summary_name(&self, key: &Key) -> String {
        estimate::row_key(key)
    }

    /// `txn`, and the put of the size summaries `rows`.
    #[must_use]
    pub fn put_estimates(&self, mut txn: Txn, rows: &[Row]) -> Txn {
        for (key, summary) in rows {
            txn = txn.put(estimate::row_key(key), estimate::encode(summary), Cond::Any);
        }
        txn
    }

    /// The size summaries the store holds. A row that cannot be read is skipped, and logged.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn estimates(&self, store: &dyn Store) -> Result<Vec<Row>, StateError> {
        let mut rows = Vec::new();
        for (key, row) in store.scan_prefix(ESTIMATE)? {
            match estimate::decode(&key, &row.value) {
                Some(row) => rows.push(row),
                None => tracing::warn!(%key, "a size summary row cannot be read: skipped"),
            }
        }
        Ok(rows)
    }
}

fn subject_key(operation: OperationId) -> String {
    format!("{SUBJECT}{:016x}", operation.0)
}

fn corrupt(key: &str, why: &str) -> StateError {
    StateError::Corrupt {
        key: key.to_owned(),
        why: why.to_owned(),
    }
}

/// The value of the row at `key`, if there is one and `check` accepts it.
fn row_at(
    store: &dyn Store,
    key: &str,
    check: fn(&[u8]) -> Result<(), &'static str>,
) -> Result<Option<Vec<u8>>, StateError> {
    let Some(row) = store.get(key)? else {
        return Ok(None);
    };
    check(&row.value).map_err(|why| corrupt(key, why))?;
    Ok(Some(row.value))
}

fn is_subject(row: &[u8]) -> Result<(), &'static str> {
    subject::decode(row).map(drop)
}

fn is_action(row: &[u8]) -> Result<(), &'static str> {
    named::decode(row).map(drop)
}

impl Ext for KbfExt {
    fn submitted(&self, txn: Txn, operation: OperationId, parcel: &[u8]) -> Txn {
        txn.put(subject_key(operation), parcel.to_vec(), Cond::Absent)
    }

    fn settled(&self, txn: Txn, operation: OperationId) -> Txn {
        txn.delete(subject_key(operation), Cond::Any)
    }

    fn ended(&self, txn: Txn, operation: OperationId, parcel: &[u8], ttl: Duration) -> Txn {
        txn.put_ttl(
            named::doneof_key(operation),
            parcel.to_vec(),
            ttl,
            Cond::Any,
        )
    }

    fn parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError> {
        row_at(store, &subject_key(operation), is_subject)
    }

    fn parcels(&self, store: &dyn Store) -> Result<BTreeMap<OperationId, Vec<u8>>, StateError> {
        let mut parcels = BTreeMap::new();
        for (key, row) in store.scan_prefix(SUBJECT)? {
            let operation = keys::operation_of(SUBJECT, &key)?;
            is_subject(&row.value).map_err(|why| corrupt(&key, why))?;
            parcels.insert(operation, row.value);
        }
        Ok(parcels)
    }

    fn ended_parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError> {
        row_at(store, &named::doneof_key(operation), is_action)
    }
}
