//! The enrollment registry in the state store (`docs/design/install-and-enrollment.md`,
//! sections 5.1 and 5.6 to 5.9 and 8.2): enrollment tokens, the node registry, and the
//! deny rows revocation writes. Pure logic over a [`kbf_state::Store`]: it opens no
//! socket, draws no randomness, signs nothing and reads the clock only through
//! [`kbf_state::Store::now`]. The frontend's enroll service (which validates CSRs and
//! owns the signer) calls it.
//!
//! **Keys.**
//!
//! | key | value | lives |
//! |---|---|---|
//! | `enroll/token/<id>` | the token: hash of its secret, roles, uses, expiry, policy | for ever |
//! | `enroll/node/<id>` | the node: its certificates, advertise host, state | for ever |
//! | `enroll/serial/<hex>` | `{node_id, role, not_after, superseded_at}` | for ever |
//! | `enroll/key/<spki sha256>` | `{node_id, role}`: one key, one node, once | for ever |
//! | `enroll/adv/<host:port>` | `{node_id}`: an advertised address belongs to one node | while the node holds it |
//! | `enroll/req/<token id>/<csr sha256>` | `{node_id}`: the idempotency row | the token's expiry plus a day |
//! | `enroll/renew/<node id>/<csr sha256>` | seen marker | 10 minutes |
//! | `deny/<kind>/<value>` | `{reason, at, by}`, kinds `node`, `serial`, `spki-sha256` | for ever |
//! | `enroll/ca/generation` | count of CA bundle changes | for ever |
//!
//! Nothing here is under `member/`, `instance/` or `down/`, so the membership scans never
//! see these rows.
//!
//! **Discipline.** Every write is one transaction and carries the version (or the absence)
//! of every row it decided on, so a racing writer makes it fail and the call decides again
//! on fresh rows, at most [`ATTEMPTS`] times. No row is cached between calls, so a handle
//! that has lost the writer role ([`kbf_state::Fenced`]) changes nothing: its writes fail
//! and the call answers [`kbf_types::RefusalReason::NotSigner`].
//!
//! **The secret.** The row holds `SHA-256` of the 32 secret bytes. The enroll
//! transaction reads the token row, then hashes the presented secret whether or not the
//! row exists, and compares the two 32-byte digests with [`subtle::ConstantTimeEq`]; an
//! unknown id is compared against a fixed digest, so it and a wrong secret do the same
//! work and give the same answer.
//!
//! **TODO(P0 amendment, plan default 3):** the `issued_at` attribute OID the CA reads and
//! the frontend checks belongs in `kbf-types`; this crate takes `issued_at` as a number and
//! defines no OID.

mod enroll;
mod error;
mod host;
mod nodes;
mod renew;
mod rows;
mod tokens;

/// Reads and commits tried before a call gives up on writers it keeps racing.
pub const ATTEMPTS: usize = 16;

pub use enroll::{EnrollRequest, Enrolled, IssuedRole, StoredCert, enroll_txn, request_hash};
pub use error::RegistryError;
pub use nodes::{DenyRecord, NodeInfo, NodeState, deny_rows, list_nodes, revoke_node};
pub use renew::{
    ISSUED_AT_WINDOW, MAX_EXPIRED_AGE, MIN_INTERVAL, REPLAY_TTL, RenewCsr, RenewRequest, renew_txn,
};
pub use tokens::{
    DEFAULT_TTL, MAX_TTL, MAX_USES, TokenInfo, TokenSpec, create_token, list_tokens, revoke_token,
};

use kbf_state::{Cond, Store, Txn, Versioned};

use crate::rows::CA_GENERATION;

fn generation_of(stored: Option<&Versioned>) -> Result<u64, RegistryError> {
    let Some(stored) = stored else {
        return Ok(0);
    };
    let bytes: [u8; 8] = stored
        .value
        .as_slice()
        .try_into()
        .map_err(|_| RegistryError::Corrupt("ca generation"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// The count of CA bundle changes (0 before the first). A node or frontend compares it with
/// the one it last saw.
///
/// # Errors
/// The store failing; a value that is not eight bytes.
pub fn ca_generation(store: &dyn Store) -> Result<u64, RegistryError> {
    generation_of(store.get(CA_GENERATION)?.as_ref())
}

/// Adds one to the count of CA bundle changes and returns the new count.
///
/// # Errors
/// As [`ca_generation`]; [`RegistryError::Contended`].
pub fn bump_ca_generation(store: &dyn Store) -> Result<u64, RegistryError> {
    for _ in 0..ATTEMPTS {
        let stored = store.get(CA_GENERATION)?;
        let next = generation_of(stored.as_ref())? + 1;
        let cond = stored.map_or(Cond::Absent, |s| Cond::Version(s.version));
        if store
            .txn(Txn::new().put(CA_GENERATION, next.to_be_bytes(), cond))?
            .is_ok()
        {
            return Ok(next);
        }
    }
    Err(RegistryError::Contended)
}
