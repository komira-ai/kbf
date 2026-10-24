//! The generation row of a store that several frontends share, and the marker [`share`]
//! leaves in it.

use crate::store::Store;
use crate::types::{Cond, StoreError, Txn};

/// The row that held a writer's generation in older builds: eight bytes, big-endian. A store
/// that several frontends share holds [`SHARED_WRITERS`] there instead ([`share`]).
pub const GENERATION: &str = "meta/generation";

/// What the generation row holds in a store that several frontends share ([`share`]). It is
/// not a generation, and not eight bytes, so a build that reads the row as a generation finds
/// it corrupt and does not start on the store: a build that would fence the shared writers
/// cannot be started beside them.
pub const SHARED_WRITERS: &[u8] = b"shared-writers";

/// How often [`share`] reads the row and tries to write it before it gives up. A try fails
/// only when another process wrote the row between the read and the write, and the other
/// writers write the same value, so a second try ends it; the rest is room for an older build
/// that takes generations while the first start runs.
const SHARE_ATTEMPTS: usize = 8;

/// Whether `bytes` are a generation as older builds wrote it: eight bytes.
fn is_generation(bytes: &[u8]) -> bool {
    bytes.len() == 8
}

/// Makes the generation row of `store` hold [`SHARED_WRITERS`], which is how a store is
/// opened by frontends that do not take its generation: none of them is fenced by another,
/// and a build that takes generations is refused by the row.
///
/// A store whose row holds the marker is left as it is (nothing is written). A store with no
/// row gets the marker with an absent-only write, and one whose row holds a generation gets
/// it in place of the generation, conditional on the version read: so frontends that open
/// the store together agree, and the row ends holding the marker. A row that changes between
/// the read and the write is read again. A row that holds neither the marker nor a
/// generation (eight bytes) is not replaced.
///
/// # Errors
/// [`StoreError`]: the store failed, the row holds neither the marker nor a generation, or
/// the row changed under every one of the tries.
pub fn share(store: &dyn Store) -> Result<(), StoreError> {
    for _ in 0..SHARE_ATTEMPTS {
        let cond = match store.get(GENERATION)? {
            Some(row) if row.value == SHARED_WRITERS => return Ok(()),
            Some(row) if !is_generation(&row.value) => {
                return Err(StoreError::Failed(
                    "the generation row is neither a generation nor the shared marker".to_owned(),
                ));
            }
            Some(row) => Cond::Version(row.version),
            None => Cond::Absent,
        };
        if store
            .txn(Txn::new().put(GENERATION, SHARED_WRITERS, cond))?
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(StoreError::Failed(
        "the generation row kept changing while it was set to the shared marker".to_owned(),
    ))
}
