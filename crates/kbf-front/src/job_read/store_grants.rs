//! The tierstore read grants `GET /locate` hands out with its table, so that a task can read the
//! packs and large objects the table names straight from the store.
//!
//! A [`StoreGrants`] holds the grant key (the same one tierstore's `--grant-key-file` holds), the
//! base URL a task is to use for the store and the bucket, all from the frontend's own flags:
//! nothing of a request reaches them. `GET /locate` gives it the object keys of the lines it is
//! about to send, which come from the grant's scope walk and from nowhere else, and gets back
//! tokens that list those keys and no others.
//!
//! **Bounds.** A token lists at most [`KEYS_PER_GRANT`] keys, and at most
//! [`BODY_BYTES_PER_GRANT`] bytes of names, so that it is well within the 64 KiB tierstore takes
//! in a header; an answer with more keys than that carries several grants, each line naming the
//! index of the one that lists its key. A key that several lines share (the spans of one pack) is
//! listed once. A grant ends at the earlier of the job's own read grant and [`GRANT_LIFETIME`]
//! from now, so a task never holds a store credential longer than it may read.
//!
//! A key tierstore refuses to list (anything under `_journal/` or `_tiering/`, or a name that is
//! not an object key) is left out, and the lines that name it are answered `absent`.
//!
//! A token is a bearer secret: it lives only in the answer's body. This type has no token in it,
//! and its `Debug` shows neither the key's secret nor anything of a token.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use tierstore::server::{AccessKey, GrantError, mint_grant};

/// How long a store grant lasts at most, from the moment it is minted.
///
/// The store refuses a grant that ends more than its `--grant-max-lifetime` (900 seconds unless
/// changed) after its own clock, and every blob would then be read from the listener. Ten
/// minutes leaves five minutes for this host's clock to run ahead of the store's.
pub const GRANT_LIFETIME: Duration = Duration::from_secs(600);

/// The most keys one grant lists.
pub const KEYS_PER_GRANT: usize = 600;

/// The most bytes of names one grant lists, a newline after each included: its token is about
/// four thirds of this, under the 64 KiB tierstore takes.
pub const BODY_BYTES_PER_GRANT: usize = 46_000;

/// Whether `url` is a base URL a task can append `/<bucket>/<key>` to: `http://` or `https://`,
/// then a host and perhaps a port. It refuses what kbf-task's client refuses in an endpoint
/// (anything but visible ASCII, and `?`, `#`, `@` for user information, and `\`), so that a
/// frontend does not start with a store URL every task would then turn down, and a path.
///
/// # Errors
/// What is wrong with it.
pub fn check_store_url(url: &str) -> Result<(), &'static str> {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return Err("must start with http:// or https://");
    };
    for c in rest.chars() {
        if !c.is_ascii_graphic() || c == '?' || c == '#' || c == '@' || c == '\\' {
            return Err(
                "has a character a store address does not: a space, a control or non-ASCII \
                 character, or one of ? # @ and a backslash",
            );
        }
    }
    let host = rest.trim_end_matches('/');
    if host.is_empty() || host.contains('/') {
        return Err("must be a host and a port with no path");
    }
    Ok(())
}

/// Whether `bucket` is a name kbf-task's client takes: 1 to 63 lowercase letters, digits, `-`
/// and `.`, not starting or ending with `.`. Tierstore takes more names than that; one of them
/// would make every task refuse the store, so the frontend refuses it at start.
///
/// # Errors
/// What is wrong with it.
pub fn check_store_bucket(bucket: &str) -> Result<(), &'static str> {
    let chars = bucket
        .bytes()
        .all(|b| b.is_ascii_lowercase() | b.is_ascii_digit() | matches!(b, b'-' | b'.'));
    let ends = bucket.starts_with('.') | bucket.ends_with('.');
    if (1..=63).contains(&bucket.len()) & chars & !ends {
        return Ok(());
    }
    Err("must be 1 to 63 of a-z, 0-9, - and ., not starting or ending with a dot")
}

/// What `GET /locate` signs store grants with, and where it says the store is.
#[derive(Debug)]
pub struct StoreGrants {
    key: AccessKey,
    store: String,
    bucket: String,
    keys_per_grant: usize,
    body_bytes: usize,
}

/// The grants minted for one answer: the tokens, and which one lists each key.
#[derive(Default)]
pub(super) struct Minted {
    tokens: Vec<String>,
    at: HashMap<String, usize>,
}

impl Minted {
    /// The index of the grant that lists `key`, if one does.
    pub(super) fn grant_of(&self, key: &str) -> Option<usize> {
        self.at.get(key).copied()
    }
}

impl StoreGrants {
    /// Grants signed with `key` for the bucket `bucket`, which a task finds at `store` (a base
    /// URL, passed on as given).
    ///
    /// # Errors
    /// [`GrantError`] when tierstore would refuse the key or the bucket.
    pub fn new(key: AccessKey, store: &str, bucket: &str) -> Result<Self, GrantError> {
        // The key and the bucket are checked once, here, by minting a grant for them.
        mint_grant(&key, bucket, 0, ["probe"])?;
        Ok(Self {
            key,
            store: store.to_owned(),
            bucket: bucket.to_owned(),
            keys_per_grant: KEYS_PER_GRANT,
            body_bytes: BODY_BYTES_PER_GRANT,
        })
    }

    /// These grants with `keys` and `body_bytes` for the most one grant lists (the defaults are
    /// [`KEYS_PER_GRANT`] and [`BODY_BYTES_PER_GRANT`]). Not a flag: a test lowers them.
    #[must_use]
    pub fn with_bounds(mut self, keys: usize, body_bytes: usize) -> Self {
        self.keys_per_grant = keys;
        self.body_bytes = body_bytes;
        self
    }

    /// The header line of an answer that carries `minted`.
    pub(super) fn header(&self, minted: &Minted) -> serde_json::Value {
        serde_json::json!({
            "store": self.store,
            "bucket": self.bucket,
            "grants": minted.tokens,
        })
    }

    /// Grants that list `keys` and end at the unix time `expires`, split by the bounds.
    pub(super) fn mint(&self, keys: &BTreeSet<String>, expires: u64) -> Minted {
        let mut minted = Minted::default();
        let mut chunk: Vec<&str> = Vec::new();
        let mut bytes = 0;
        for key in keys {
            let full = chunk.len() >= self.keys_per_grant;
            if !chunk.is_empty() && (full || bytes + key.len() + 1 > self.body_bytes) {
                self.seal(&mut minted, std::mem::take(&mut chunk), expires);
                bytes = 0;
            }
            bytes += key.len() + 1;
            chunk.push(key);
        }
        self.seal(&mut minted, chunk, expires);
        minted
    }

    /// Adds the grant of `chunk` to `minted`, leaving out the keys tierstore refuses to list. A
    /// chunk that cannot be made into a grant at all adds none, and its lines are `absent`.
    fn seal(&self, minted: &mut Minted, mut chunk: Vec<&str>, expires: u64) {
        while !chunk.is_empty() {
            match mint_grant(&self.key, &self.bucket, expires, chunk.iter().copied()) {
                Ok(token) => {
                    for key in chunk {
                        minted.at.insert(key.to_owned(), minted.tokens.len());
                    }
                    minted.tokens.push(token);
                    return;
                }
                Err(GrantError::Name { name, .. }) => chunk.retain(|key| *key != name),
                Err(error) => {
                    tracing::warn!(%error, keys = chunk.len(), "a store read grant could not be made");
                    return;
                }
            }
        }
    }
}
