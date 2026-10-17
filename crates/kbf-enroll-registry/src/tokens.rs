//! Tokens: create, list, revoke. The row layout is design section 5.1.

use std::time::Duration;

use kbf_state::{Cond, Store, Txn};
use kbf_types::{NodeId, RoleSet, TokenId, TokenSecret};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

use crate::ATTEMPTS;
use crate::error::{Corrupt, RegistryError};
use crate::rows::{self, TOKEN_PREFIX, TokenRow};

/// The longest a token lives.
pub const MAX_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a token lives when its creator names no time.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
/// The most uses a token may have.
pub const MAX_USES: u32 = 1000;

/// What an operator asks of a new token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSpec {
    /// The roles it grants.
    pub roles: RoleSet,
    /// How many nodes it can enroll: 1 to [`MAX_USES`].
    pub max_uses: u32,
    /// How long it lives from now: more than zero, at most [`MAX_TTL`]; [`DEFAULT_TTL`]
    /// when `None`.
    pub ttl: Option<Duration>,
    /// The one node id it may enroll.
    pub node_id: Option<NodeId>,
    /// The prefix every node id it enrolls must carry.
    pub node_prefix: Option<String>,
    /// Advertise hosts it allows: a host, or a suffix that starts with `.`. Empty allows
    /// any.
    pub names: Vec<String>,
    /// Whether it may enroll a node id that already exists.
    pub replace: bool,
    /// For the operator.
    pub label: String,
    /// The API caller's label.
    pub created_by: String,
}

/// A token as the operator sees it. Never the secret or its hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    /// Its lookup id.
    pub id: TokenId,
    /// The roles it grants.
    pub roles: RoleSet,
    /// Uses allowed.
    pub max_uses: u32,
    /// Uses consumed.
    pub uses: u32,
    /// Store-clock seconds at which it expires.
    pub expires_at: u64,
    /// Store-clock seconds at which it was created.
    pub created_at: u64,
    /// Store-clock seconds at which it was revoked.
    pub revoked_at: Option<u64>,
    /// The exact node id it allows.
    pub node_id: Option<String>,
    /// The node id prefix it requires.
    pub node_prefix: Option<String>,
    /// The advertise names it allows.
    pub names: Vec<String>,
    /// Whether it may replace a node.
    pub replace: bool,
    /// For the operator.
    pub label: String,
    /// The API caller's label.
    pub created_by: String,
}

/// `SHA-256` of presented secret bytes: what the row keeps.
pub(crate) fn secret_digest(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

pub(crate) fn info(id: &TokenId, row: &TokenRow) -> Result<TokenInfo, Corrupt> {
    Ok(TokenInfo {
        id: id.clone(),
        roles: rows::role_set(&row.roles, "token")?,
        max_uses: row.max_uses,
        uses: row.uses,
        expires_at: row.expires_at,
        created_at: row.created_at,
        revoked_at: row.revoked_at,
        node_id: row.node_id.clone(),
        node_prefix: row.node_prefix.clone(),
        names: row.names.clone(),
        replace: row.replace,
        label: row.label.clone(),
        created_by: row.created_by.clone(),
    })
}

/// Stores a new token whose secret is `secret`. The caller drew `id` and `secret` from a
/// CSPRNG and keeps the secret to show once; the store keeps its hash.
///
/// # Errors
/// [`RegistryError::Invalid`] for a spec out of range or an id already in use; the store
/// failing.
pub fn create_token(
    store: &dyn Store,
    id: &TokenId,
    secret: &TokenSecret,
    spec: &TokenSpec,
) -> Result<TokenInfo, RegistryError> {
    unimplemented!("stub: the red-first commit has signatures only")
}

/// Every token, by id. The secret is not in the answer.
///
/// # Errors
/// The store failing; a row that does not decode.
pub fn list_tokens(store: &dyn Store) -> Result<Vec<TokenInfo>, RegistryError> {
    unimplemented!("stub: the red-first commit has signatures only")
}

/// Sets `revoked_at` on a token, which stops new enrollments with it; nodes already
/// enrolled stay enrolled. Revoking a revoked token changes nothing.
///
/// # Errors
/// [`RegistryError::NotFound`] for an unknown id; the store failing;
/// [`RegistryError::Contended`].
pub fn revoke_token(store: &dyn Store, id: &TokenId) -> Result<(), RegistryError> {
    unimplemented!("stub: the red-first commit has signatures only")
}
