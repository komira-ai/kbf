//! Tokens: create, list, revoke. The row layout is design section 5.1.

use std::time::Duration;

use kbf_state::{Cond, Store, Txn};
use kbf_types::{NodeId, RoleSet, TokenId, TokenSecret};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

use crate::ATTEMPTS;
use crate::error::{Corrupt, RegistryError};
use crate::host::{is_name_rule, is_node_prefix};
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
/// [`RegistryError::Invalid`] for a spec out of range (`names` that are no host names, a
/// `node_prefix` no node id can start with included) or an id already in use; the store
/// failing.
pub fn create_token(
    store: &dyn Store,
    id: &TokenId,
    secret: &TokenSecret,
    spec: &TokenSpec,
) -> Result<TokenInfo, RegistryError> {
    if !(1..=MAX_USES).contains(&spec.max_uses) {
        return Err(RegistryError::Invalid("max_uses must be 1 to 1000"));
    }
    if !spec.names.iter().all(|name| is_name_rule(name)) {
        return Err(RegistryError::Invalid(
            "names must be host names, addresses or .suffixes",
        ));
    }
    if !spec.node_prefix.as_deref().is_none_or(is_node_prefix) {
        return Err(RegistryError::Invalid(
            "node_prefix must be 1 to 63 of a-z, 0-9 and -, not starting with -",
        ));
    }
    let ttl = spec.ttl.unwrap_or(DEFAULT_TTL);
    if ttl < Duration::from_secs(1) || ttl > MAX_TTL {
        return Err(RegistryError::Invalid(
            "expires_in must be at least a second and at most 30 days",
        ));
    }
    let now = rows::seconds(store.now());
    let row = TokenRow {
        secret_sha256: secret_digest(secret.as_bytes()).to_vec(),
        roles: spec.roles.iter().map(|r| r.name().to_owned()).collect(),
        max_uses: spec.max_uses,
        uses: 0,
        expires_at: now + ttl.as_secs(),
        node_id: spec.node_id.as_ref().map(ToString::to_string),
        node_prefix: spec.node_prefix.clone(),
        names: spec.names.clone(),
        replace: spec.replace,
        label: spec.label.clone(),
        created_at: now,
        created_by: spec.created_by.clone(),
        revoked_at: None,
    };
    let txn = Txn::new().put(rows::token_key(id), row.encode_to_vec(), Cond::Absent);
    if store.txn(txn)?.is_err() {
        return Err(RegistryError::Invalid("a token with this id exists"));
    }
    Ok(info(id, &row)?)
}

/// Every token, by id. The secret is not in the answer.
///
/// # Errors
/// The store failing; a row that does not decode.
pub fn list_tokens(store: &dyn Store) -> Result<Vec<TokenInfo>, RegistryError> {
    let mut out = Vec::new();
    for (key, stored) in store.scan_prefix(TOKEN_PREFIX)? {
        let id = key
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|text| TokenId::parse(text).ok())
            .ok_or(Corrupt("token key"))?;
        out.push(info(&id, &rows::decode(&stored, "token")?)?);
    }
    Ok(out)
}

/// Sets `revoked_at` on a token, which stops new enrollments with it; nodes already
/// enrolled stay enrolled. Revoking a revoked token changes nothing.
///
/// # Errors
/// [`RegistryError::NotFound`] for an unknown id; the store failing;
/// [`RegistryError::Contended`].
pub fn revoke_token(store: &dyn Store, id: &TokenId) -> Result<(), RegistryError> {
    let key = rows::token_key(id);
    for _ in 0..ATTEMPTS {
        let stored = store.get(&key)?.ok_or(RegistryError::NotFound)?;
        let mut row: TokenRow = rows::decode(&stored, "token")?;
        if row.revoked_at.is_some() {
            return Ok(());
        }
        row.revoked_at = Some(rows::seconds(store.now()));
        let txn = Txn::new().put(&key, row.encode_to_vec(), Cond::Version(stored.version));
        if store.txn(txn)?.is_ok() {
            return Ok(());
        }
    }
    Err(RegistryError::Contended)
}
