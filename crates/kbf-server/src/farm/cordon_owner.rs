//! Who owns a node's cordon: the `cordon-owner/<node>` row, read for `GET /v1/nodes/{id}`.
//!
//! The owner is a row of its own beside the `cordon/<node>` row, not part of it: a frontend
//! that does not know of owners reads `cordon/` rows as a tag byte and a deadline and
//! refuses anything else as corrupt, and it never looks at a key it does not name. A reader
//! of owners therefore works beside any writer, old or new.
//!
//! The value is UTF-8: `operator`, or `updater:<unit>` with a unit that is not empty. No
//! row, a row that is not one of those, or a row that cannot be read, is the operator's:
//! nobody but the operator lifts a cordon whose owner is not known. A row that is not one of
//! those is logged and is not a corrupt state.
//!
//! Nothing writes the row yet; the writers (`Farm::place` on either core) still write the
//! `cordon/` row alone.

use kbf_state::{Store, StoreError};
use kbf_types::WorkerId;

use crate::fleet::{NodeDetail, Owner, PlacementView};

/// The key family of the rows.
const CORDON_OWNER: &str = "cordon-owner/";

/// The key of `worker`'s row.
pub(super) fn key(worker: &WorkerId) -> String {
    format!("{CORDON_OWNER}{worker}")
}

/// The owner a row's `value` names; `None` if it names none.
pub(super) fn parse(value: &[u8]) -> Option<Owner> {
    match std::str::from_utf8(value).ok()? {
        "operator" => Some(Owner::Operator),
        text => match text.strip_prefix("updater:")? {
            "" => None,
            unit => Some(Owner::Updater(unit.to_owned())),
        },
    }
}

/// The owner of `worker`'s cordon in `store`: the operator if there is no row or the row
/// is malformed (which is logged).
///
/// # Errors
/// The store cannot be read.
pub(super) fn read(store: &dyn Store, worker: &WorkerId) -> Result<Owner, StoreError> {
    let Some(row) = store.get(&key(worker))? else {
        return Ok(Owner::Operator);
    };
    Ok(parse(&row.value).unwrap_or_else(|| {
        tracing::warn!(
            %worker,
            bytes = row.value.len(),
            "the cordon owner row is malformed: the operator's"
        );
        Owner::Operator
    }))
}

/// [`read`] for a farm that may have no store, which logs a store that cannot be read and
/// takes the owner for the operator's.
pub(super) fn shown(store: Option<&dyn Store>, worker: &WorkerId) -> Owner {
    let Some(store) = store else {
        return Owner::Operator;
    };
    read(store, worker).unwrap_or_else(|error| {
        tracing::warn!(%worker, %error, "the cordon owner could not be read: the operator's");
        Owner::Operator
    })
}

/// `detail` with the owner of its cordon, `read` when the node is out of placement.
pub(super) fn with_owner(mut detail: NodeDetail, read: impl FnOnce() -> Owner) -> NodeDetail {
    if detail.node.placement != PlacementView::Serving {
        detail.owner = Some(read());
    }
    detail
}
